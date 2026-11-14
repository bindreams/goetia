//! A task is two plists, the job `<id>.plist` and the timer `<id>@timer.plist`; this module is the pure
//! part of keeping them as one: what a pair of file names holds, and what that makes of the id.
//! Not `#[cfg]`-gated, so the table below is tested on every host; the effectful reading that feeds it
//! is `manager::task`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::backend::Decoded;

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

#[cfg(test)]
#[path = "task_tests.rs"]
mod task_tests;
