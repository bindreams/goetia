//! A task is two plists, the job `<id>.plist` and the timer `<id>@timer.plist`; this module is the pure
//! part of keeping them as one: what a pair of file names holds, and what that makes of the id.
//! Not `#[cfg]`-gated, so the table below is tested on every host; the effectful reading that feeds it
//! is `manager::task`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::backend::Decoded;
use crate::backend::launchd::generate::Plists;
use crate::backend::launchd::refuse::changes_loaded_job;
use crate::spec::{DaemonKind, DaemonSpec};

/// What one file name (`<id>.plist` or `<id>@timer.plist`) holds in the two directories a plist can
/// be in.
// `Marked` holds a whole `Blob`, as `ReadOutcome::Marked` does through its snapshot; same call.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum Slot {
    /// Neither directory has it.
    Absent,
    /// One directory has it, and it is goetia's: the marker is there, and its metadata decoded or
    /// failed to.
    Marked(Decoded),
    /// One directory has it, and it is not goetia's (no marker, a binary plist, bytes that are not
    /// UTF-8, or not a regular file), or both do and neither is goetia's.
    NotOurs,
    /// Both directories have it, and at least one is goetia's: a state no operation of goetia
    /// leaves, which no verb may pick a copy of.
    Both,
}

/// What a task's two slots make of its id.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Classified {
    /// Nothing of goetia's, and nothing at either name.
    Absent,
    /// Not goetia's to act on. `timer` says it is the timer that is not goetia's, which a task needs
    /// for its schedule, so the recovery names the timer's path.
    Foreign { timer: bool },
    /// Goetia's, with whatever of the two plists is goetia's.
    Marked,
    /// Goetia's, and not usable: the reason is what `list` says of it, and `uninstall` is the way out.
    Unusable(&'static str),
}

/// The reason for a timer whose job is not there.
pub(crate) const JOB_MISSING: &str = "the timer's job is missing";

/// The reason for a slot that is in both directories.
pub(crate) const IN_BOTH: &str =
    "present in both /Library/Application Support/Goetia/daemons and /Library/LaunchDaemons";

/// What a task's job and timer slots make of the id. `job_is_task` is whether the job's metadata
/// names a task: `Some(true)` for a task, `Some(false)` for a service, `None` where the metadata will
/// not decode (or there is no job).
///
/// | job | timer | result |
/// |---|---|---|
/// | absent | absent | [`Classified::Absent`] |
/// | marked | absent | marked, no timer |
/// | marked | marked | marked, with both |
/// | a task | not goetia's | foreign, recovery names the timer |
/// | a service, or a blob that will not decode | not goetia's | marked, the timer ignored |
/// | absent | not goetia's | foreign, recovery names the timer |
/// | absent | marked | unusable: [`JOB_MISSING`] |
/// | not goetia's | any | foreign |
/// | in both directories | any | unusable: [`IN_BOTH`] |
/// | any | in both directories | unusable: [`IN_BOTH`], where the job is a task or absent |
///
/// A timer that is not goetia's, or is in both directories, is ignored beside a job that is a service
/// or will not decode, so such a job reads as it did before a task had a timer to ask about.
pub(crate) fn classify(job: &Slot, timer: &Slot, job_is_task: Option<bool>) -> Classified {
    let task = job_is_task == Some(true);
    match (job, timer) {
        (Slot::NotOurs, _) => Classified::Foreign { timer: false },
        (Slot::Both, _) => Classified::Unusable(IN_BOTH),
        (Slot::Absent, Slot::Absent) => Classified::Absent,
        (Slot::Absent, Slot::NotOurs) => Classified::Foreign { timer: true },
        (Slot::Absent, Slot::Marked(_)) => Classified::Unusable(JOB_MISSING),
        (Slot::Absent, Slot::Both) => Classified::Unusable(IN_BOTH),
        (Slot::Marked(_), Slot::Absent | Slot::Marked(_)) => Classified::Marked,
        (Slot::Marked(_), Slot::NotOurs) if task => Classified::Foreign { timer: true },
        (Slot::Marked(_), Slot::Both) if task => Classified::Unusable(IN_BOTH),
        (Slot::Marked(_), Slot::NotOurs | Slot::Both) => Classified::Marked,
    }
}

// Updating and uninstalling ===========================================================================================

/// One step of changing the files and jobs of an installed task, in the order a plan gives them.
///
/// `Action`, not `Step`: [`crate::manager::Step`] (`Install`, `Start`, `Stop`, ...) already names the
/// verbs a reservation counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Create the timer's plist where the job's is (staging, once the job is moved there), refusing to
    /// replace what is at that name.
    CreateTimerFirst,
    BootoutTimer,
    /// Read the job again, now that no timer can start it: a service that is loaded, or a task with a
    /// run in flight, is refused with [`crate::error::Error::LoadedJob`], the timer re-armed first.
    CheckJobIdle,
    BootoutJob,
    /// Move the job's plist from the enabled directory to staging, so that a retyped daemon does not
    /// start at boot as the other kind.
    MoveJobToStaging,
    ReplaceJob,
    ReplaceTimer,
    RemoveTimer,
    RemoveJob,
    BootstrapJob,
    BootstrapTimer,
}

/// A plist as installed: its text, and whether it is in the enabled directory.
#[derive(Debug, Clone)]
pub(crate) struct OnDisk {
    pub(crate) enabled: bool,
    pub(crate) text: String,
}

/// An installed task, or a daemon about to become one, as pure facts: what is on disk and what launchd
/// holds. What `update_plan` and `uninstall_plan` read.
#[derive(Debug, Clone)]
pub(crate) struct Old {
    /// The kind the installed job's metadata names.
    pub(crate) kind: DaemonKind,
    /// `None` for a timer alone at the id.
    pub(crate) job: Option<OnDisk>,
    pub(crate) timer: Option<OnDisk>,
    /// Whether launchd has the job's label, and the timer's. A run in flight is not a fact here:
    /// whether the job has one is read by [`Action::CheckJobIdle`], after the timer is out, since a
    /// fire between this read and the bootout could start one.
    pub(crate) job_loaded: bool,
    pub(crate) timer_loaded: bool,
}

impl Old {
    /// Where a task is enabled: its timer's directory, and the job's for a task with no timer.
    pub(crate) fn enabled(&self) -> bool {
        self.timer
            .as_ref()
            .or(self.job.as_ref())
            .is_some_and(|plist| plist.enabled)
    }
}

/// What `install` does to an installed daemon's plists and jobs to make them `new` (rendered from
/// `spec`), where either the old or the new is a task. launchd keeps the definition a job was loaded
/// with and has no way to update it, so a plist launchd holds is booted out, replaced and loaded again.
///
/// - The timer goes out first, if it is loaded and anything it or the job holds changes: no fire can
///   start a run meanwhile.
/// - A job whose rendering changes and that is loaded is checked (see [`Action::CheckJobIdle`]) before
///   anything is written, so a refusal leaves the installed files untouched; an idle one is booted out.
///   A change that touches only the timer leaves the job alone, so a run in flight never blocks a
///   schedule edit.
/// - A timer that is new is created before the job's files change, no-clobber, so a stranger's file at
///   its name refuses with nothing changed; one that is dropped is removed before the job is replaced.
/// - A retype moves an enabled job to staging first: it drops arming and boot enrolment.
/// - A task that is enabled ends with its job and its timer loaded, the job first: what went out is
///   loaded again, a new timer is armed, and a plist launchd did not hold (an install that failed
///   after booting it out) is loaded now.
///
/// A file is replaced when its text differs at all, metadata included.
pub(crate) fn update_plan(old: &Old, new: &Plists, spec: &DaemonSpec) -> Vec<Action> {
    let job = old.job.as_ref().expect("an update acts on an id that has a job");
    let retype = spec.kind() != old.kind;
    let job_renders_differently = changes_loaded_job(&job.text, &new.job);
    let added = old.timer.is_none() && new.timer.is_some();
    let dropped = old.timer.is_some() && new.timer.is_none();
    let (timer_differs, timer_renders_differently) = match (&old.timer, &new.timer) {
        (Some(old), Some(new)) => (old.text != *new, changes_loaded_job(&old.text, new)),
        _ => (false, false),
    };
    let job_out = old.job_loaded && job_renders_differently;
    let timer_out = old.timer_loaded && (dropped || timer_renders_differently || job_out);
    // A retype leaves nothing enabled.
    let armed = !retype && old.enabled();

    let mut plan = Vec::new();
    let mut push = |condition: bool, action: Action| {
        if condition {
            plan.push(action);
        }
    };
    push(timer_out, Action::BootoutTimer);
    push(job_out, Action::CheckJobIdle);
    push(added, Action::CreateTimerFirst);
    push(job_out, Action::BootoutJob);
    push(dropped, Action::RemoveTimer);
    push(retype && job.enabled, Action::MoveJobToStaging);
    push(job.text != new.job, Action::ReplaceJob);
    push(timer_differs, Action::ReplaceTimer);
    // An enabled task is loaded (`enable` loads it), so whatever launchd does not hold afterwards is
    // loaded, whether this plan booted it out or an earlier install that failed did.
    let job_held = old.job_loaded && !job_out;
    let timer_held = old.timer_loaded && !timer_out;
    push(armed && !job_held, Action::BootstrapJob);
    push(armed && new.timer.is_some() && !timer_held, Action::BootstrapTimer);
    plan
}

/// What `uninstall` does: boot out the timer, then the job (each only if launchd holds it, and the
/// job's boot-out ends a run in flight), then remove the timer's file, then the job's. A crash
/// anywhere leaves a state the read table names, and running the verb again finishes it.
pub(crate) fn uninstall_plan(old: &Old) -> Vec<Action> {
    let mut plan = Vec::new();
    let mut push = |condition: bool, action: Action| {
        if condition {
            plan.push(action);
        }
    };
    push(old.timer_loaded, Action::BootoutTimer);
    push(old.job_loaded, Action::BootoutJob);
    push(old.timer.is_some(), Action::RemoveTimer);
    push(old.job.is_some(), Action::RemoveJob);
    plan
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod task_tests;
