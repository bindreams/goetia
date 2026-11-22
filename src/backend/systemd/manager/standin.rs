//! The stand-in unit that keeps a left run alive: see [`crate::manager::ServiceManager::uninstall`] and
//! [`generate::standin`] for what it is and why a run still in its start job needs one.
//!
//! One `systemctl show` of the job's unit decides what `uninstall` does about the run (see [`UnitRun`]):
//! with [`RunInFlight::Leave`], a run in its start job gets a stand-in, a run past it is left alone, and no
//! run is the plain uninstall, whose stop cancels a restart that is waiting. Nothing is decided from the unit
//! file's text or from what goetia reads the job to be.
//!
//! Its life: nothing removes it while its run is in flight. The next `install` or `uninstall` of the id, the
//! absent id included, removes it once there is no run, after a `reset-failed` for a left run that failed, and
//! reloads. With [`RunInFlight::Stop`] `uninstall` stops the run first. goetia reads `/etc/systemd/system`
//! for fragments, so a stand-in never makes an id read as installed (see `discover`).

use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::systemd::generate;
use crate::error::{Error, Result};
use crate::manager::{Budget, RunInFlight};

use super::discover::{RawState, classify_and_read};
use super::systemctl::{Held, RunPhase, UnitRun};
use super::write::{CreateOutcome, ReplaceOutcome, create_unit_in, replace_unit_verified_in};
use super::{Systemd, UnitKind, remove_file_if_present, unit_name, unit_path};

/// Where systemd reads runtime unit files: a tmpfs, so a stand-in is gone at a reboot.
pub(super) const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";

/// Where `id`'s stand-in is written. `/etc/systemd/system` outranks it, so an installed unit of the same
/// name is the definition systemd uses.
pub(super) fn standin_path(id: &str) -> PathBuf {
    Path::new(RUNTIME_UNIT_DIR).join(unit_name(id, UnitKind::Service))
}

/// What is at `id`'s stand-in path.
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
    match classify_and_read(&standin_path(id)).map_err(super::discover::ReadFailure::into_os_error)? {
        RawState::Regular(text) if generate::standin_id(&text).as_deref() == Some(id) => Ok(StandinFile::Ours(text)),
        RawState::Regular(_) | RawState::NotOurs => Ok(StandinFile::NotOurs),
        RawState::Absent => Ok(StandinFile::Absent),
    }
}

/// What `uninstall_units` does about the stand-in, decided before anything is removed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Standin {
    /// Nothing: no run to keep, no stand-in of ours, or a stand-in whose run is still going.
    Nothing,
    /// Write it, replacing one of ours that is there, before the files go and the manager reloads. The two
    /// values are what the read printed.
    Write { kill_signal: String, stop_timeout: String },
    /// Remove the one that is there, whose run is over, after clearing its failed run if it has one
    /// (`reset-failed` refuses a unit that is not in memory, as one that did not fail is not).
    Remove { failed: bool },
}

/// The one rule for a stand-in of ours that is found, whichever of `uninstall`, `uninstall` of an absent id
/// and `install` found it: it goes when there is no run, and with [`RunInFlight::Stop`] there is none by
/// now (the caller stopped it first); with [`RunInFlight::Leave`] a run in flight keeps it.
fn found(in_flight: RunInFlight, run: &UnitRun) -> Standin {
    match (in_flight, run.phase) {
        (RunInFlight::Leave, RunPhase::InStartJob | RunPhase::Running) => Standin::Nothing,
        _ => Standin::Remove { failed: run.failed },
    }
}

/// What a release of a stand-in left: whether it reloaded the manager, and the read it was decided from.
pub(super) struct Released {
    pub reloaded: bool,
    pub run: Option<UnitRun>,
}

impl Systemd {
    /// The refusal of [`RunInFlight::Leave`] when a file that is not goetia's is at the stand-in's path:
    /// raised whether or not a run is in flight now, and before anything is changed.
    pub(super) fn refuse_blocked(&self, id: &str, file: &StandinFile) -> Result<()> {
        match file {
            StandinFile::NotOurs => Err(Error::StandinBlocked {
                id: id.to_string(),
                path: standin_path(id),
            }),
            StandinFile::Absent | StandinFile::Ours(_) => Ok(()),
        }
    }

    /// What `uninstall_units` does about the stand-in of `snapshot`'s id, from `run`: the read it made first
    /// for `Leave`, none for `Stop` (which stopped the run, and reads what is left only if there is a stand-in
    /// to settle).
    pub(super) fn plan_standin(
        &self,
        id: &str,
        in_flight: RunInFlight,
        file: &StandinFile,
        run: Option<UnitRun>,
    ) -> Result<Standin> {
        match run {
            Some(run) if in_flight == RunInFlight::Leave && run.phase == RunPhase::InStartJob => Ok(Standin::Write {
                kill_signal: run.kill_signal,
                stop_timeout: run.stop_timeout,
            }),
            _ if file.is_ours() => {
                let run = match run {
                    Some(run) => run,
                    None => self.systemctl.run_of(&unit_name(id, UnitKind::Service))?,
                };
                Ok(found(in_flight, &run))
            }
            _ => Ok(Standin::Nothing),
        }
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
    /// caller. `reset-failed` comes first: systemd refuses it for a unit it has no file for.
    pub(super) fn remove_standin(&self, id: &str, failed: bool, held: &mut Held) -> Result<()> {
        if failed {
            self.systemctl.reset_failed(id, UnitKind::Service, held)?;
        }
        remove_file_if_present(&standin_path(id))
    }

    /// The part of `install` that concerns a stand-in left at the id: a stand-in whose run is over is
    /// removed and the manager reloaded, and one with a run in flight stays, as an install never stops a run.
    /// Asks nothing of systemd where there is no stand-in of ours.
    pub(super) fn release_standin(&self, id: &str, held: &mut Held) -> Result<()> {
        self.release(id, RunInFlight::Leave, held, false).map(drop)
    }

    /// `uninstall` of an id nothing is installed at. A stand-in of ours at it is settled by the one rule
    /// (`Stop` stops its run first). And one read says whether systemd still holds a definition loaded from
    /// goetia's own path whose file is gone, which a `Leave` uninstall whose last reload failed leaves, and
    /// which restarts a unit from memory when its process exits: it is reloaded before the answer, with `Stop`
    /// and with `Leave` alike. A reload that fails is the call's error, not "not installed".
    pub(super) fn uninstall_leftovers(&self, id: &str, in_flight: RunInFlight, held: &mut Held) -> Result<()> {
        let released = self.release(id, in_flight, held, true)?;
        let held_definition = released
            .run
            .is_some_and(|run| run.loaded_from(&unit_path(id, UnitKind::Service)));
        if held_definition && !released.reloaded {
            self.systemctl.daemon_reload(held)?;
        }
        Ok(())
    }

    /// The stand-in of ours at `id`, settled by the one rule: with `Stop` the run is stopped first, then,
    /// and with `Leave` when there is no run, the stand-in is removed and the manager reloaded. `always_read`
    /// reads the unit's run even when there is no stand-in.
    fn release(&self, id: &str, in_flight: RunInFlight, held: &mut Held, always_read: bool) -> Result<Released> {
        let present = standin_file(id)?.is_ours();
        if !present && !always_read {
            return Ok(Released {
                reloaded: false,
                run: None,
            });
        }
        self.systemctl.require_supported(held)?;
        if present && in_flight == RunInFlight::Stop {
            // Unbounded, as `uninstall`'s own stop.
            self.systemctl
                .stop_impl(id, Budget::Unbounded, Budget::Unbounded.start(), held)?;
        }
        let run = self.systemctl.run_of(&unit_name(id, UnitKind::Service))?;
        if present && let Standin::Remove { failed } = found(in_flight, &run) {
            self.remove_standin(id, failed, held)?;
            self.systemctl.daemon_reload(held)?;
            return Ok(Released {
                reloaded: true,
                run: Some(run),
            });
        }
        Ok(Released {
            reloaded: false,
            run: Some(run),
        })
    }
}

#[cfg(test)]
#[path = "standin_tests.rs"]
mod standin_tests;
