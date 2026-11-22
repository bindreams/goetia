//! The stand-in unit that keeps a left run alive: see [`crate::manager::ServiceManager::uninstall`] and
//! [`generate::standin`] for what it is and why a task's run needs one.
//!
//! Its life: `uninstall` with [`RunInFlight::Leave`] writes it for a task's run that is in flight, before
//! the unit files go and the manager reloads. Nothing removes it while its unit is active. The next
//! `install` or `uninstall` of the id, the absent id included, removes it once its unit is not active,
//! after a `reset-failed` that clears a left run that failed, and reloads. `uninstall` without `Leave`
//! stops the run first. goetia reads `/etc/systemd/system` for fragments, so a stand-in never makes an id
//! read as installed (see `discover`).

use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::InstalledService as _;
use crate::backend::systemd::generate;
use crate::error::{Error, Result};
use crate::manager::{Budget, RunInFlight, State};

use super::discover::{RawState, classify_and_read};
use super::systemctl::Held;
use super::task::Fragment;
use super::write::{CreateOutcome, create_unit_in};
use super::{Systemd, UnitKind, remove_file_if_present, unit_name};

/// Where systemd reads runtime unit files: a tmpfs, so a stand-in is gone at a reboot.
pub(super) const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";

/// Where `id`'s stand-in is written. `/etc/systemd/system` outranks it, so an installed unit of the same
/// name is the definition systemd uses.
pub(super) fn standin_path(id: &str) -> PathBuf {
    Path::new(RUNTIME_UNIT_DIR).join(unit_name(id, UnitKind::Service))
}

/// What `uninstall_units` does about the stand-in, decided before anything is removed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Standin {
    /// Nothing: no run to keep, no stand-in of ours, or a run another stand-in already keeps.
    Nothing,
    /// Write it for the run of the job whose text is this, before the files go and the manager reloads.
    Write(String),
    /// Remove the one that is there, whose unit is not active.
    Remove,
}

/// Whether the file at `id`'s stand-in path is a stand-in of `id`'s own. Anything else there is somebody's
/// runtime unit and is never touched.
fn is_ours(id: &str) -> Result<bool> {
    match classify_and_read(&standin_path(id)).map_err(super::discover::ReadFailure::into_os_error)? {
        RawState::Regular(text) => Ok(generate::standin_id(&text).as_deref() == Some(id)),
        RawState::Absent | RawState::NotOurs => Ok(false),
    }
}

impl Systemd {
    /// What to do about the stand-in of `snapshot`'s id when it is uninstalled with `in_flight`. With
    /// `Leave` this is the one read of whether a run is in flight, after the timer is disarmed: a task's
    /// job that is running gets a stand-in, and a stand-in whose unit is not active goes. The read is
    /// made only where its answer can matter: a task or a job with a timer of goetia's, or a stand-in of
    /// ours at the id.
    ///
    /// A run somebody else starts between this read and the reload is that caller's own conflicting
    /// action, and nothing guards it.
    pub(super) fn plan_standin(&self, snapshot: &Fragment, in_flight: RunInFlight, held: &mut Held) -> Result<Standin> {
        let id = snapshot.id().as_str();
        let present = is_ours(id)?;
        match in_flight {
            // The run is stopped, so a stand-in left by an earlier `Leave` has nothing to keep.
            RunInFlight::Stop => Ok(if present { Standin::Remove } else { Standin::Nothing }),
            RunInFlight::Leave => {
                let task_like = snapshot.owns_timer_slot();
                if !task_like && !present {
                    return Ok(Standin::Nothing);
                }
                self.systemctl.require_supported(held)?;
                Ok(match (self.run_in_flight(id)?, snapshot.service_text()) {
                    (true, Some(job)) if task_like => Standin::Write(job.to_string()),
                    (true, _) => Standin::Nothing,
                    (false, _) if present => Standin::Remove,
                    (false, _) => Standin::Nothing,
                })
            }
        }
    }

    /// Whether `id`'s job is in its start job or running: a unit systemd has no file for is not.
    fn run_in_flight(&self, id: &str) -> Result<bool> {
        match self.systemctl.task_state(&unit_name(id, UnitKind::Service)) {
            Ok(job) => Ok(job.state == State::Running),
            Err(Error::UnitNotLoaded { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The stand-in for the run of `id`, whose removed job was `job`, written without replacing a file that
    /// is not one of goetia's. One of ours already there (the left run of an earlier `Leave`) is what is
    /// wanted.
    pub(super) fn write_standin(&self, id: &str, job: &str) -> Result<()> {
        fs::create_dir_all(RUNTIME_UNIT_DIR).map_err(|e| Error::os_at("create", Path::new(RUNTIME_UNIT_DIR), e))?;
        match create_unit_in(
            Path::new(RUNTIME_UNIT_DIR),
            id,
            UnitKind::Service,
            &generate::standin(id, job),
        )? {
            CreateOutcome::Created => Ok(()),
            CreateOutcome::Raced if is_ours(id)? => Ok(()),
            CreateOutcome::Raced => Err(Error::ConcurrentChange {
                id: id.to_string(),
                path: Some(standin_path(id)),
            }),
        }
    }

    /// The stand-in of `id` cleared of a failed left run and removed; the manager is reloaded by the caller.
    /// `reset-failed` comes first: systemd refuses it for a unit it has no file for.
    pub(super) fn remove_standin(&self, id: &str, held: &mut Held) -> Result<()> {
        self.systemctl.reset_failed(id, UnitKind::Service, held)?;
        remove_file_if_present(&standin_path(id))
    }

    /// The part of `install` and of `uninstall` of an id nothing installed that concerns a stand-in left
    /// there: `Stop` stops its run first, `Leave` leaves a stand-in whose unit is active, and a stand-in
    /// whose unit is not active is removed and the manager reloaded. Asks nothing of systemd where there is
    /// no stand-in of ours.
    pub(super) fn release_standin(&self, id: &str, in_flight: RunInFlight, held: &mut Held) -> Result<()> {
        if !is_ours(id)? {
            return Ok(());
        }
        self.systemctl.require_supported(held)?;
        match in_flight {
            RunInFlight::Stop => {
                // Unbounded, as `uninstall`'s own stop.
                self.systemctl
                    .stop_impl(id, Budget::Unbounded, Budget::Unbounded.start(), held)?;
            }
            RunInFlight::Leave if self.run_in_flight(id)? => return Ok(()),
            RunInFlight::Leave => {}
        }
        self.remove_standin(id, held)?;
        self.systemctl.daemon_reload(held)
    }
}

#[cfg(test)]
#[path = "standin_tests.rs"]
mod standin_tests;
