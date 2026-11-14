//! A task is two plists in one directory, the job `<id>.plist` and the timer `<id>@timer.plist`;
//! this module reads them as one id (`read` and `scan` share [`assemble`]) and reports them as one
//! daemon (`live_status`, `task_state`). What a pair of files makes of an id is decided purely, in
//! [`crate::backend::launchd::task`]; this is what feeds it and what is built from its answer.
//!
//! A directory pair holds each of the two file names (a *slot*), so [`locate`] and [`Location`] serve
//! both: a slot is read from every location it was found at, and the slots are classified together.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use jiff::tz::TimeZone;

use super::{Launchctl, Launchd, Location, Plist, locate, read_artifact};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::launchd::generate::{self, timer_label};
use crate::backend::launchd::state;
use crate::backend::launchd::task::{Classified, Slot, classify};
use crate::backend::{Decoded, ReadOutcome, Unnamed};
use crate::error::{Error, Result};
use crate::manager::budget::Deadline;
use crate::manager::{Budget, DaemonStatus, Exit, State, Status, TaskStatus};
use crate::spec::{DaemonKind, DaemonSpec, Id, next_fire, system_zone};

// Reading =============================================================================================================

/// A plist file as `read` saw it: where it is, and its text.
pub(super) struct Artifact {
    pub(super) location: Location,
    pub(super) text: String,
}

impl Artifact {
    /// The metadata of a marked file. Only a marked file is an artifact, so the marker is there, and
    /// only a damaged one is `Err`.
    pub(super) fn blob(&self) -> Decoded {
        match generate::extract(&self.text) {
            Ok(Some(blob)) => Ok(blob),
            Ok(None) => Err(Arc::new(Error::blob("the plist has no goetia metadata"))),
            Err(e) => Err(Arc::new(e)),
        }
    }
}

/// What `read` or `scan` saw of an id's two plists, beyond its blob.
pub(super) struct Body {
    /// `None` where the job is missing, which leaves a timer alone at the id.
    pub(super) job: Option<Artifact>,
    /// The timer, where there is one that is goetia's.
    pub(super) timer: Option<Artifact>,
}

/// What is at one location.
// `Marked` holds a whole `Blob`, as `ReadOutcome::Marked` does through its snapshot; same call.
#[allow(clippy::large_enum_variant)]
enum Contents {
    NotOurs,
    Marked(Decoded, Artifact),
}

/// What is at `location`: no marker is [`Contents::NotOurs`], a marker is `Marked` whether or not its
/// blob decodes, since the marker alone is proof of ownership. Whether the spec can still be rendered
/// here is `flow`'s question, answered with `render`.
fn read_at(id: &str, location: Location) -> Result<Contents> {
    let text = read_artifact(&location.path, id)?;
    Ok(match generate::extract(&text).transpose() {
        None => Contents::NotOurs,
        Some(blob) => Contents::Marked(blob.map_err(Arc::new), Artifact { location, text }),
    })
}

/// One file name, read at every location it was found at.
struct SlotRead {
    slot: Slot,
    /// The file, where the slot is [`Slot::Marked`].
    artifact: Option<Artifact>,
    /// Where it was found: what a recovery names.
    paths: Vec<PathBuf>,
}

impl SlotRead {
    fn of(slot: Slot) -> Self {
        Self {
            slot,
            artifact: None,
            paths: Vec::new(),
        }
    }
}

/// `locations`, the places one file name was found, read: every read that did not complete is a
/// failure, so that neither ownership nor absence is claimed over it. A file gone since it was found
/// (an uninstall completed) is absent.
///
/// A name found in both places is [`Slot::Both`] when either is goetia's, and otherwise not goetia's:
/// only a marker makes an id goetia's.
fn read_slot(id: &str, locations: Vec<Location>) -> std::result::Result<SlotRead, Vec<Error>> {
    let paths: Vec<PathBuf> = locations.iter().map(|location| location.path.clone()).collect();
    let mut found = Vec::new();
    let mut failed = Vec::new();
    for location in locations {
        match read_at(id, location) {
            Ok(contents) => found.push(contents),
            // Gone since the name was found: absence is established, so skip; reporting it would let a
            // benign concurrent uninstall raise `list`'s exit code.
            Err(e) if matches!(e.cause(), Error::NotInstalled { .. }) => {}
            Err(e) => failed.push(e),
        }
    }
    if !failed.is_empty() {
        return Err(failed);
    }
    let mut read = SlotRead::of(Slot::Absent);
    read.paths = paths;
    match <[Contents; 1]>::try_from(found) {
        Ok([Contents::Marked(blob, artifact)]) => {
            read.slot = Slot::Marked(blob);
            read.artifact = Some(artifact);
        }
        Ok([Contents::NotOurs]) => read.slot = Slot::NotOurs,
        Err(found) if found.is_empty() => {}
        Err(found) => {
            let marked = found.iter().any(|contents| matches!(contents, Contents::Marked(..)));
            read.slot = if marked { Slot::Both } else { Slot::NotOurs };
        }
    }
    Ok(read)
}

/// An id's two slots, classified.
// `Read` holds a whole `ReadOutcome`, which holds a snapshot's `Blob`; same call.
#[allow(clippy::large_enum_variant)]
pub(super) enum Assembled {
    /// As `read` answers it.
    Read(ReadOutcome<Unnamed<Body>>),
    /// Goetia's, and not usable (see [`Classified::Unusable`]): `read` makes it a snapshot whose blob is
    /// the reason, which `uninstall` can still act on; `scan` reports the reason.
    Unusable { reason: &'static str, body: Body },
    /// A task goetia installed, whose timer's name a stranger's file holds. `read` and the verbs call
    /// the id foreign, with `recovery`; `scan` reports it as goetia's and unusable, for `list` is not
    /// to lose a task that is installed and may be running.
    ForeignTimer { recovery: String },
}

/// What the job at `job` and the timer at `timer()` (asked only where it can change the answer, so
/// a stranger's job never has its neighbour looked at) make of `id`. The failures of the reads that
/// did not complete are the `Err`.
///
/// A timer that cannot be read counts as not goetia's where the job is a service or will not decode,
/// so such a job reads as it did before a task had a timer to ask about.
pub(super) fn assemble(
    id: &str,
    job: Vec<Location>,
    timer: impl FnOnce() -> Result<Vec<Location>>,
) -> std::result::Result<Assembled, Vec<Error>> {
    let job = read_slot(id, job)?;
    let job_is_task = match &job.slot {
        Slot::Marked(Ok(blob)) => Some(blob.spec.kind() == DaemonKind::Task),
        _ => None,
    };
    let timer = match job.slot {
        Slot::NotOurs | Slot::Both => SlotRead::of(Slot::Absent),
        _ => match timer()
            .map_err(|e| vec![e])
            .and_then(|locations| read_slot(id, locations))
        {
            Ok(timer) => timer,
            Err(_) if matches!(job.slot, Slot::Marked(_)) && job_is_task != Some(true) => SlotRead::of(Slot::NotOurs),
            Err(failures) => return Err(failures),
        },
    };

    let class = classify(&job.slot, &timer.slot, job_is_task);
    let body = Body {
        job: job.artifact,
        timer: timer.artifact,
    };
    Ok(match class {
        Classified::Absent => Assembled::Read(ReadOutcome::Absent),
        Classified::Foreign { timer: false } => Assembled::Read(ReadOutcome::Foreign { recovery: None }),
        Classified::Foreign { timer: true } => {
            let recovery = foreign_timer_recovery(&timer.paths);
            if matches!(job.slot, Slot::Marked(_)) {
                Assembled::ForeignTimer { recovery }
            } else {
                Assembled::Read(ReadOutcome::Foreign {
                    recovery: Some(recovery),
                })
            }
        }
        Classified::Marked => {
            let Slot::Marked(blob) = job.slot else {
                unreachable!("`classify` answers `Marked` for a marked job only");
            };
            Assembled::Read(ReadOutcome::Marked(Unnamed { blob, body }))
        }
        Classified::Unusable(reason) => Assembled::Unusable { reason, body },
    })
}

/// How to free the timer's name, which a task needs for its schedule.
pub(super) fn foreign_timer_recovery(paths: &[PathBuf]) -> String {
    let paths: Vec<String> = paths.iter().map(|path| format!("`{}`", path.display())).collect();
    format!(
        "{} is not goetia's, and a task needs that name for its schedule: remove or rename it, and re-run",
        paths.join(" and ")
    )
}

/// What `Backend::read` answers for `id` over `dirs`.
pub(super) fn read_in(dirs: super::Dirs, id: &Id) -> Result<ReadOutcome<Plist>> {
    let name = id.as_str();
    let locations = |stem: &str| locate(dirs, name, stem).map(|found| found.into_iter().collect::<Vec<_>>());
    let assembled = assemble(name, locations(name)?, || locations(&timer_label(name))).map_err(single)?;
    Ok(match assembled {
        Assembled::Read(found) => found.named(id, Plist::new),
        Assembled::ForeignTimer { recovery } => ReadOutcome::Foreign {
            recovery: Some(recovery),
        },
        Assembled::Unusable { reason, body } => {
            ReadOutcome::Marked(Plist::new(id.clone(), Err(Arc::new(Error::blob(reason))), body))
        }
    })
}

/// The failures of one read, as the one error `read` returns.
fn single(failures: Vec<Error>) -> Error {
    Error::both_all(failures).expect("a read that failed has a failure")
}

// Status ==============================================================================================================

/// What one `print` said of a task's job.
pub(super) struct TaskPrint {
    pub(super) state: State,
    pub(super) pid: Option<u32>,
    /// A start launchd has accepted and not yet forked (`spawn scheduled`): `Running`, with no pid.
    pub(super) queued: bool,
    pub(super) last_result: Option<Exit>,
}

impl Launchctl {
    /// One `print` of the task `id`'s job as `role`, read as a task's (see [`state::classify_task`]): a
    /// job launchd does not hold is idle. `Err` iff `launchctl` could not be run at all; anything else
    /// it answers that is not recognised is `Unknown`, as for a service.
    pub(super) fn read_task_print(
        &self,
        id: &str,
        deadline: Deadline,
        role: Role,
        reservation: &mut Reservation,
    ) -> Result<TaskPrint> {
        let Some((status, capture)) = self.read_print(id, deadline, role, reservation)? else {
            return Ok(TaskPrint {
                state: State::Unknown,
                pid: None,
                queued: false,
                last_result: None,
            });
        };
        let (state, pid) = super::classified(status, &capture, DaemonKind::Task);
        let whole = status.success() && capture.complete;
        let body = String::from_utf8_lossy(&capture.stdout);
        let last_result = whole.then(|| state::last_result(&body)).flatten();
        Ok(TaskPrint {
            state,
            pid,
            queued: whole && state::spawn_scheduled(&body),
            last_result,
        })
    }
}

impl Launchd {
    /// A task's status: its job's state and last result from one `print`, whether its timer is
    /// enabled (the timer's directory; the job's for a task with no timer), and when the armed timer
    /// fires next. `last_run` is not known: launchd keeps no time of it.
    pub(super) fn task_status(&self, snapshot: &Plist) -> Result<Status> {
        // `status` and `list` have no budget of their own, and neither counted this read.
        let print = self.launchctl.read_task_print(
            snapshot.id.as_str(),
            Budget::Unbounded.start(),
            Role::UncountedQuery,
            &mut Reservation::default(),
        )?;
        let enabled = snapshot
            .timer
            .as_ref()
            .or(snapshot.job.as_ref())
            .is_some_and(|artifact| artifact.location.enabled);
        let next_run = next_run(snapshot.timer.as_ref(), enabled, SystemTime::now(), system_zone)?;
        Ok(Status::Task(TaskStatus {
            common: DaemonStatus {
                state: print.state,
                pid: print.pid,
                enabled,
            },
            last_result: print.last_result,
            last_run: None,
            next_run,
        }))
    }

    /// The run state of a task, from `print` alone, so that a part of the status that can fail (the
    /// system's time zone) cannot refuse a `run`.
    pub(super) fn task_run_state(&self, snapshot: &Plist) -> Result<State> {
        self.launchctl
            .read_task_print(
                snapshot.id.as_str(),
                Budget::Unbounded.start(),
                Role::UncountedQuery,
                &mut Reservation::default(),
            )
            .map(|print| print.state)
    }
}

/// When `timer` fires first after `now`, read from the schedule in the timer's own metadata, not the
/// job's: the armed timer is what fires, and after a crash between replacing the job and the timer the
/// job's metadata can hold a schedule the timer does not. `None` unless `armed`, and for a timer whose
/// metadata names no schedule or will not decode, or whose schedule has no date. The zone is asked
/// for only when there is a schedule to read, so a disarmed task needs no time zone.
pub(super) fn next_run(
    timer: Option<&Artifact>,
    armed: bool,
    now: SystemTime,
    zone: impl FnOnce() -> Result<TimeZone>,
) -> Result<Option<SystemTime>> {
    let Some(timer) = timer.filter(|_| armed) else {
        return Ok(None);
    };
    let Ok(blob) = timer.blob() else {
        return Ok(None);
    };
    let DaemonSpec::Task(task) = &blob.spec else {
        return Ok(None);
    };
    let Some(schedule) = &task.schedule else {
        return Ok(None);
    };
    Ok(next_fire(schedule, now, &zone()?))
}

#[cfg(test)]
#[path = "task_tests.rs"]
pub(super) mod task_tests;
