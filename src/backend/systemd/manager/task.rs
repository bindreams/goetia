//! A task is two artifacts, `<id>.service` (its job) and `<id>.timer` (its schedule), and this
//! module is what keeps them as one: the snapshot of both, the rule that regenerates them, and every
//! step of a write, `uninstall`, `enable`, `disable` and `status` that touches the timer.
//!
//! # The two artifacts and a crash between them
//!
//! No step of goetia is atomic over two files, so every order below is one whose interruption leaves
//! a state `read` classifies as goetia's and the next `install` repairs:
//!
//! - A create writes the job and then the timer: a crash leaves a job without a timer, which is ours
//!   and regenerates without one.
//! - An update replaces the job and then the timer. A crash between them leaves job B beside timer A,
//!   which regenerates from each artifact's own blob, so it is two unmodified artifacts, an `Update`
//!   and not a `Conflict`.
//! - A timer that is to go (a retype to a service, a schedule dropped) is disarmed and removed
//!   *before* the job is replaced: the other order leaves a timer starting a job that is no longer a
//!   task. A crash between them leaves a job and no timer, as above.
//! - A service that becomes a task is disabled *before* its job is replaced: its `[Install]` is still
//!   there to read, and the link it leaves would run the task at every boot.
//! - `uninstall` disarms the timer, stops the job, and removes the timer before the job: a crash
//!   leaves a job without a timer, still ours.
//!
//! A timer without its job (a crash of an older goetia, or a hand removal) is ours and unusable: its
//! blob is the error `read` gives it, and `uninstall` removes it.

use std::sync::Arc;

use crate::backend::systemd::generate::{self, Units};
use crate::backend::{Decoded, InstalledService, WriteOutcome};
use crate::blob::Blob;
use crate::error::{Error, Result};
use crate::manager::flow::NOTHING_TO_ARM;
use crate::manager::{Budget, DaemonStatus, Status, TaskStatus};
use crate::spec::{DaemonKind, DaemonSpec, Id};

use super::systemctl::Held;
use super::write::{CreateOutcome, ReplaceOutcome, create_unit, quarantine_if_still_ours, replace_unit_verified};
use super::{
    Systemd, UnitKind, dropin_dir, identity_for, journal, remove_dir_if_present, remove_file_if_present, unit_name,
    unit_path,
};

// Fragment ============================================================================================================

/// A marked timer, as `read` saw it.
pub(super) struct Timer {
    /// The timer's own text, drop-ins not folded in: the identity its replacement and removal verify.
    pub(super) text: String,
    /// The timer's own metadata, which the timer is regenerated from while it decodes.
    pub(super) blob: Decoded,
}

/// What `read` saw of one id beyond its blob: the job's text, and the timer.
pub(super) struct Body {
    /// `None` where the job is missing, which leaves a timer alone at the id.
    pub(super) service: Option<String>,
    pub(super) timer: Option<Timer>,
}

/// A marked id as `read` saw it: its job, and its timer where it has one that is goetia's. The text
/// of each is the identity a later write or removal verifies the file against (see
/// [`replace_unit_verified`] and [`quarantine_if_still_ours`]): content, not inode, since the kernel
/// can reuse an inode number moments after its file is unlinked.
pub(crate) struct Fragment {
    id: Id,
    /// The job's metadata, or why there is none: it will not decode, or the job is missing.
    blob: Decoded,
    body: Body,
}

impl Fragment {
    pub(super) fn new(id: Id, blob: Decoded, body: Body) -> Self {
        Self { id, blob, body }
    }

    /// The job's own text, `None` where the job is missing.
    pub(super) fn service_text(&self) -> Option<&str> {
        self.body.service.as_deref()
    }

    pub(super) fn timer(&self) -> Option<&Timer> {
        self.body.timer.as_ref()
    }

    /// Which kind of daemon the job's metadata says it is: `None` where that is not known.
    pub(super) fn kind(&self) -> Option<DaemonKind> {
        self.blob.as_ref().ok().map(|blob| blob.spec.kind())
    }

    fn is_task(&self) -> bool {
        self.kind() == Some(DaemonKind::Task)
    }

    /// Whether the timer's slot is this id's to read and clear: a timer is there, or the job is a task
    /// that gets one. A service beside no timer of goetia's never reads `<id>.timer.d`.
    pub(super) fn owns_timer_slot(&self) -> bool {
        self.timer().is_some() || self.is_task()
    }
}

impl InstalledService for Fragment {
    fn id(&self) -> &Id {
        &self.id
    }

    fn blob(&self) -> std::result::Result<&Blob, &Arc<Error>> {
        self.blob.as_ref()
    }
}

// regenerate ==========================================================================================================

/// `spec` as `snapshot` would hold it, unmodified: the job from `spec`, and the timer from its own
/// metadata where that decodes (else from `spec`), so that a job and a timer left by two different
/// installs are each unmodified. A job beside no timer regenerates without one, so a missing timer is
/// not drift: install and enable write it.
///
/// Each spec goes through [`generate::units`], which refuses what cannot be written, so a timer is
/// never generated for a spec its job could not be.
pub(super) fn regenerate(spec: &DaemonSpec, snapshot: &Fragment) -> Result<Units> {
    let mut units = generate::units(spec, &identity_for(&spec.common().user)?)?;
    units.timer = match snapshot.timer() {
        None => None,
        Some(Timer { blob: Ok(blob), .. }) => {
            generate::units(&blob.spec, &identity_for(&blob.spec.common().user)?)?.timer
        }
        Some(_) => units.timer,
    };
    Ok(units)
}

// write ===============================================================================================================

/// What an update has to do to the id before the job is replaced.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Before {
    /// The old job is a service that is to become a task: disable it, so its boot link does not run
    /// the task at every boot.
    pub(super) disable_job: bool,
    /// A timer is installed and the new definition has none: disarm and remove it.
    pub(super) drop_timer: bool,
}

pub(super) fn before_replacing(old: &Fragment, new: &Units, spec: &DaemonSpec) -> Before {
    Before {
        disable_job: old.kind() == Some(DaemonKind::Service) && spec.kind() == DaemonKind::Task,
        drop_timer: old.timer().is_some() && new.timer.is_none(),
    }
}

impl Systemd {
    /// A create: the job, then the timer, each never replacing a file; then one reload.
    pub(super) fn create_units(&self, id: &str, units: &Units, held: &mut Held) -> Result<WriteOutcome> {
        if let CreateOutcome::Raced = create_unit(id, UnitKind::Service, &units.service)? {
            return Ok(WriteOutcome::Raced);
        }
        if let Some(timer) = &units.timer {
            if let CreateOutcome::Raced = create_unit(id, UnitKind::Timer, timer)? {
                return Ok(WriteOutcome::Raced);
            }
        }
        self.systemctl.daemon_reload_or_report(id, held)?;
        Ok(WriteOutcome::Done)
    }

    /// An update or a regeneration over `old`: see the module doc for the order and why.
    pub(super) fn update_units(
        &self,
        spec: &DaemonSpec,
        units: &Units,
        old: &Fragment,
        held: &mut Held,
    ) -> Result<WriteOutcome> {
        let id = spec.common().id.as_str();
        let old_job = old
            .service_text()
            .expect("an id `decide` chose to update has a job: one without is unusable");
        let before = before_replacing(old, units, spec);

        if before.drop_timer {
            let old_timer = old.timer().expect("a timer to drop is installed");
            self.systemctl.disable_timer(id, held)?;
            match quarantine_if_still_ours(id, UnitKind::Timer, &old_timer.text)? {
                Some(backup) => remove_file_if_present(&backup)?,
                None => return Ok(WriteOutcome::Raced),
            }
        }
        if before.disable_job {
            self.systemctl.disable_impl(id, held)?;
        }

        if let ReplaceOutcome::Raced = replace_unit_verified(id, UnitKind::Service, &units.service, old_job)? {
            return Ok(WriteOutcome::Raced);
        }
        if let Some(timer) = &units.timer {
            let raced = match old.timer() {
                Some(old_timer) => matches!(
                    replace_unit_verified(id, UnitKind::Timer, timer, &old_timer.text)?,
                    ReplaceOutcome::Raced
                ),
                None => matches!(create_unit(id, UnitKind::Timer, timer)?, CreateOutcome::Raced),
            };
            if raced {
                return Ok(WriteOutcome::Raced);
            }
        }

        // Only after the last write has committed: clearing a drop-in first would delete an admin's
        // overrides even on a path that turns out `Raced` and never writes anything.
        remove_dir_if_present(&dropin_dir(id, UnitKind::Service))?;
        if units.timer.is_some() || old.timer().is_some() {
            remove_dir_if_present(&dropin_dir(id, UnitKind::Timer))?;
        }
        self.systemctl.daemon_reload_or_report(id, held)?;
        Ok(WriteOutcome::Done)
    }
}

// uninstall ===========================================================================================================

impl Systemd {
    /// The timer disarmed, the job stopped, then the timer and the job removed, each verified against
    /// what `read` saw. A plain service keeps its order: stop, disable, remove, reload.
    pub(super) fn uninstall_units(&self, snapshot: Fragment, held: &mut Held) -> Result<()> {
        let id = snapshot.id.as_str();
        let owns_timer = snapshot.owns_timer_slot();

        if snapshot.timer().is_some() {
            self.systemctl.disable_timer(id, held)?;
        }
        // Deliberately `Budget::Unbounded`: `uninstall` has no `--timeout` of its own, this stop is a
        // means rather than an end, and bounding it would turn a slow-stopping service into a failed
        // uninstall where today it succeeds.
        if snapshot.service_text().is_some() {
            self.systemctl
                .stop_impl(id, Budget::Unbounded, Budget::Unbounded.start(), held)?;
            // A task's job has no `[Install]`: there is nothing to disable.
            if !snapshot.is_task() {
                self.systemctl.disable_impl(id, held)?;
            }
        }

        // Verified removals, mirroring `replace_unit_verified`: the gap since `read` saw each file
        // spans `systemctl` round-trips, wide enough for something else to have replaced it.
        if let Some(timer) = snapshot.timer() {
            remove_verified(id, UnitKind::Timer, &timer.text)?;
        }
        if let Some(job) = snapshot.service_text() {
            remove_verified(id, UnitKind::Service, job)?;
        }

        // Both attempted regardless of whether the first failed, and every failure reported
        // together: a partial failure here must not leave systemd's loaded view silently stale.
        let mut dropins = remove_dir_if_present(&dropin_dir(id, UnitKind::Service));
        if owns_timer {
            dropins = both(dropins, remove_dir_if_present(&dropin_dir(id, UnitKind::Timer)));
        }
        match both(dropins, self.systemctl.daemon_reload(held)) {
            Err(e @ Error::Both { .. }) => Err(Error::Step {
                what: format!("uninstall for `{id}` partially failed, and the unit fragment is already gone"),
                source: Box::new(e),
            }),
            settled => settled,
        }
    }
}

/// `first` and `then`, both settled: the first failure when one failed, and both when both did.
fn both(first: Result<()>, then: Result<()>) -> Result<()> {
    match (first, then) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Err(first), Err(then)) => Err(Error::Both {
            first: Box::new(first),
            then: Box::new(then),
        }),
    }
}

/// `id`'s `kind` unit removed, if it is still the file `read` saw.
fn remove_verified(id: &str, kind: UnitKind, expected: &str) -> Result<()> {
    match quarantine_if_still_ours(id, kind, expected)? {
        Some(backup) => remove_file_if_present(&backup),
        None => Err(Error::ConcurrentChange {
            id: id.to_string(),
            path: Some(unit_path(id, kind)),
        }),
    }
}

// enable and disable ==================================================================================================

impl Systemd {
    /// A service as it always was. A task arms its schedule. A snapshot whose blob does not decode
    /// acts on the units present.
    pub(super) fn enable_units(&self, snapshot: &Fragment, held: &mut Held) -> Result<()> {
        let id = snapshot.id.as_str();
        match snapshot.kind() {
            Some(DaemonKind::Service) => self.systemctl.enable_impl(id, held),
            Some(DaemonKind::Task) => self.arm(snapshot, held),
            None => {
                if snapshot.service_text().is_some() {
                    self.systemctl.enable_impl(id, held)?;
                }
                if snapshot.timer().is_some() {
                    self.systemctl.enable_timer(id, held)?;
                }
                Ok(())
            }
        }
    }

    /// The task's timer, written first where it is missing (a task installed without one, or its
    /// timer lost), then reloaded and enabled now.
    fn arm(&self, snapshot: &Fragment, held: &mut Held) -> Result<()> {
        let id = snapshot.id.as_str();
        if snapshot.timer().is_none() {
            let blob = snapshot.blob().expect("a task is a snapshot whose blob decoded");
            // Through `units`, which refuses what cannot be written, so a timer is never written for
            // a job that could not be.
            let units = generate::units(&blob.spec, &identity_for(&blob.spec.common().user)?)?;
            let timer = units
                .timer
                .ok_or_else(|| crate::spec::invalid(snapshot.id(), NOTHING_TO_ARM))?;
            match create_unit(id, UnitKind::Timer, &timer)? {
                CreateOutcome::Created => self.systemctl.daemon_reload_or_report(id, held)?,
                CreateOutcome::Raced => {
                    return Err(Error::ConcurrentChange {
                        id: id.to_string(),
                        path: Some(unit_path(id, UnitKind::Timer)),
                    });
                }
            }
        }
        self.systemctl.enable_timer(id, held)
    }

    /// A service as it always was. A task's is its timer's, and a run in flight goes on: a task
    /// with no timer has nothing to disable.
    pub(super) fn disable_units(&self, snapshot: &Fragment) -> Result<()> {
        let id = snapshot.id.as_str();
        let held = &mut Held::default();
        match snapshot.kind() {
            Some(DaemonKind::Service) => self.systemctl.disable_impl(id, held),
            Some(DaemonKind::Task) => self.disarm(snapshot, held),
            None => {
                if snapshot.service_text().is_some() {
                    self.systemctl.disable_impl(id, held)?;
                }
                self.disarm(snapshot, held)
            }
        }
    }

    fn disarm(&self, snapshot: &Fragment, held: &mut Held) -> Result<()> {
        match snapshot.timer() {
            Some(_) => self.systemctl.disable_timer(snapshot.id.as_str(), held),
            None => Ok(()),
        }
    }
}

// status ==============================================================================================================

impl Systemd {
    /// A service's status from its unit; a task's from three places: the state of its job, whether the
    /// timer is enabled and when it elapses next, and its last run from the journal. A journal that
    /// cannot be read is the error, never a task that has not run.
    pub(super) fn status_of(&self, snapshot: &Fragment) -> Result<Status> {
        let id = snapshot.id.as_str();
        let job = unit_name(id, UnitKind::Service);
        if !snapshot.is_task() {
            return self.systemctl.status_from_unit(&job).map(Status::Service);
        }
        let (state, pid) = self.systemctl.task_state(&job)?;
        let (enabled, next_run) = match snapshot.timer() {
            None => (false, None),
            Some(_) => (
                self.systemctl.timer_enabled(id)?,
                self.busctl.next_elapse(&unit_name(id, UnitKind::Timer))?,
            ),
        };
        let last = journal::last_run(self.journal.as_ref(), &job)?;
        Ok(Status::Task(TaskStatus {
            common: DaemonStatus { state, pid, enabled },
            last_result: last.exit,
            last_run: last.started,
            next_run,
        }))
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod task_tests;
