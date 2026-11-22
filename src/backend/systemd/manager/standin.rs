//! The stand-in unit that keeps a left run alive: see [`crate::manager::ServiceManager::uninstall`] and
//! [`generate::standin`] for what it is and why a run still in its start job needs one.
//!
//! What `uninstall` and `install` do about a run and a stand-in is decided once, by [`plan::plan`] from one
//! `systemctl show` of the job's unit (see [`UnitRun`]), and carried out in one order by
//! `Systemd::settle` (in `task`). The three callers, [`Systemd::uninstall_units`], [`Systemd::uninstall_leftovers`]
//! and [`Systemd::release_standin`], only say what they are. Nothing is decided from the unit file's text or from
//! what goetia reads the job to be.
//!
//! Its life: nothing removes it while its run has a process. The next `install` or `uninstall` of the id, the
//! absent id included, removes it once there is none, after a `reset-failed` for a left run that failed, and
//! reloads. With [`RunInFlight::Stop`] `uninstall` stops the run first. goetia reads `/etc/systemd/system`
//! for fragments, so a stand-in never makes an id read as installed (see `discover`).

mod plan;

use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::systemd::generate;
use crate::error::{Error, Result};
use crate::manager::RunInFlight;

use super::discover::{RawState, ReadFailure, classify_and_read};
use super::systemctl::Held;
use super::task::Fragment;
use super::write::{CreateOutcome, ReplaceOutcome, create_unit_in, replace_unit_verified_in};
use super::{Systemd, UnitKind, remove_file_if_present, unit_name, unit_path};

pub(super) use plan::{Clearing, Holding, Known, Seen, Standin, plan};

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
    /// `uninstall` of an installed id, whose snapshot this call removes.
    Installed(&'a Fragment),
    /// `uninstall` of an id nothing is installed at: it looks for a definition systemd still holds whose file
    /// is gone.
    Sweep,
    /// `install`, which settles a stand-in it finds as [`RunInFlight::Leave`] does.
    Install,
}

impl Systemd {
    /// The one read of `id`'s job unit a call makes, when [`Known::reads`] says it makes one. A read asks no
    /// version gate. For a call that looks for a held definition, whether systemd holds one loaded from the
    /// id's own path whose file is gone.
    pub(super) fn look(&self, id: &str, known: &Known<'_>) -> Result<Option<Seen>> {
        if !known.reads() {
            return Ok(None);
        }
        let run = self.systemctl.run_of(&unit_name(id, UnitKind::Service))?;
        let path = unit_path(id, UnitKind::Service);
        let held_gone = known.asks_held
            && run.loaded_from(&path)
            && matches!(
                classify_and_read(&path).map_err(ReadFailure::into_os_error)?,
                RawState::Absent
            );
        Ok(Some(Seen { run, held_gone }))
    }

    /// The stand-in for the run of `id`, written without replacing a file that is not one of goetia's: a
    /// stand-in of ours already there is replaced, since its values are an earlier job's. A file that is
    /// not ours, which appeared after the check, is [`Error::ConcurrentChange`].
    pub(super) fn write_standin(
        &self,
        id: &str,
        file: &StandinFile,
        kill_signal: &str,
        stop_timeout: &str,
    ) -> Result<()> {
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
        Ok(())
    }

    /// The stand-in of `id` removed, after a failed left run is cleared; the manager is reloaded by the
    /// caller.
    pub(super) fn remove_standin(&self, id: &str, failed: bool, held: &mut Held) -> Result<()> {
        if failed {
            self.systemctl.reset_failed(id, UnitKind::Service, held)?;
        }
        remove_file_if_present(&standin_path(id))
    }

    /// `install`'s part: a stand-in of ours at the id whose run has no process is removed and the manager
    /// reloaded, and one with a process stays, as an install never stops a run. Asks nothing of systemd where
    /// there is no stand-in of ours.
    pub(super) fn release_standin(&self, id: &str, held: &mut Held) -> Result<()> {
        self.settle(id, RunInFlight::Leave, Target::Install, held)
    }

    /// `uninstall` of an id nothing is installed at: a stand-in of ours is settled by the one plan, and a
    /// definition systemd still holds from the id's own path whose file is gone (a `Leave` uninstall whose
    /// last reload failed, which restarts a unit from memory when its process exits) is reloaded, under either
    /// choice. Where no manager can be asked nothing is read and the answer stays "not installed". A reload
    /// that fails is the call's error, not "not installed".
    pub(super) fn uninstall_leftovers(&self, id: &str, in_flight: RunInFlight, held: &mut Held) -> Result<()> {
        self.settle(id, in_flight, Target::Sweep, held)
    }
}

#[cfg(test)]
#[path = "standin_tests.rs"]
mod standin_tests;
