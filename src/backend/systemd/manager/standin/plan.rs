//! What `uninstall` and `install` do about a run in flight and a stand-in, decided here and nowhere else.
//!
//! [`plan`] takes everything known and returns everything to do; `Systemd::settle` reads the facts, calls it
//! and carries the plan out in one order. The rules, each written once:
//!
//! 1. [`RunInFlight::Leave`] never stops: a stop sent after a read of a restart that is waiting could land on
//!    a process systemd started in between.
//! 2. [`RunInFlight::Stop`] stops when goetia holds a job or a stand-in of its own is there, and never
//!    otherwise.
//! 3. A stand-in is written (or one of ours replaced) only for `Leave`, a job goetia removes, and a run still
//!    in its start job: with the two values the read printed. Never for a timer alone or an id nothing is
//!    installed at, whose unit may be somebody else's.
//! 4. A stand-in of ours that is not written goes when the run has no process once the stop is done, after
//!    `reset-failed` when the run failed; it stays while the run has a process. Removing is followed by the
//!    reload.
//! 5. A file at the stand-in's path that is not ours refuses `Leave` on an installed id, before anything is
//!    changed ([`Known::blocked`]); with `Stop`, and where nothing is installed, it is left alone.
//! 6. `Leave` over a restart that is waiting, with files removed: after the reload the unit is read again,
//!    and cleared with `reset-failed` if it failed (the restart fails on the missing unit).
//! 7. Where nothing is installed, and a manager can be asked, a definition systemd holds from the id's own
//!    path whose file is gone is reloaded, under either choice, and nothing else is done for it.
//! 8. `install` applies rule 4 to a stand-in it finds, and nothing else of this.

use crate::manager::RunInFlight;

use super::StandinFile;
use crate::backend::systemd::manager::systemctl::{RunPhase, UnitRun};

/// What goetia holds of the id, as far as this call removes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend::systemd::manager) enum Holding {
    /// An installed id whose snapshot has the job's own file, which this call removes.
    Job,
    /// An installed id whose snapshot has no job file: a timer alone.
    NoJob,
    /// Nothing is removed: `uninstall` of an id nothing is installed at, and `install`.
    Absent,
}

/// What is known before the run is read.
#[derive(Debug)]
pub(in crate::backend::systemd::manager) struct Known<'a> {
    pub in_flight: RunInFlight,
    pub holding: Holding,
    pub file: &'a StandinFile,
    /// The call is `uninstall` of an id nothing is installed at, which looks for a held definition (rule 7).
    pub asks_held: bool,
}

/// What the one read of the job's unit showed.
#[derive(Debug)]
pub(in crate::backend::systemd::manager) struct Seen {
    pub run: UnitRun,
    /// Systemd holds a definition loaded from the id's own path, and that file does not exist.
    pub held_gone: bool,
}

/// What a stand-in of ours at the id comes to.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::backend::systemd::manager) enum Standin {
    /// Nothing: no stand-in of ours, or one whose run still has a process.
    Nothing,
    /// Write it, replacing one of ours that is there, before the files go. The two values are what the read
    /// printed.
    Write { kill_signal: String, stop_timeout: String },
    /// Remove it, after the run's failure is cleared where there is one.
    Remove { clearing: Clearing },
}

/// Whether the failed run of a stand-in that goes is cleared with `reset-failed` first (the verb refuses a
/// unit that is not in memory, as one that did not fail is not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend::systemd::manager) enum Clearing {
    /// The read says the run did not fail.
    Not,
    /// The read says it failed.
    Needed,
    /// The plan stops the run, which is what ends it: whether it failed is read once the stop is done, and
    /// cleared if it did.
    // Red: not planned yet.
    #[allow(dead_code)]
    ReadAfterStop,
}

/// Everything to do, in the order `Systemd::settle` does it: the stop, the stand-in, the reload.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::backend::systemd::manager) struct Plan {
    pub stop: bool,
    pub standin: Standin,
    /// The manager is reloaded once, after the files and the stand-in are dealt with.
    pub reload: bool,
}

impl Known<'_> {
    /// Rule 5: this call is refused before anything is changed.
    pub(in crate::backend::systemd::manager) fn blocked(&self) -> bool {
        self.in_flight == RunInFlight::Leave
            && self.holding != Holding::Absent
            && matches!(self.file, StandinFile::NotOurs)
    }

    /// Whether the call reads the job's unit: `Leave` over an installed id, a stand-in of ours to settle, or
    /// the look for a held definition. `Stop` over an installed id with no stand-in of ours, and an `install`
    /// with none, read nothing.
    pub(in crate::backend::systemd::manager) fn reads(&self) -> bool {
        (self.in_flight == RunInFlight::Leave && self.holding != Holding::Absent)
            || self.file.is_ours()
            || self.asks_held
    }
}

/// The plan for a call that is not [`Known::blocked`]. `seen` is `Some` exactly when [`Known::reads`].
pub(in crate::backend::systemd::manager) fn plan(known: &Known<'_>, seen: Option<&Seen>) -> Plan {
    debug_assert!(!known.blocked());
    debug_assert_eq!(known.reads(), seen.is_some());
    let leave = known.in_flight == RunInFlight::Leave;
    let ours = known.file.is_ours();
    let removes_files = known.holding != Holding::Absent;
    let stop = !leave && (known.holding == Holding::Job || ours);
    let Some(seen) = seen else {
        return Plan {
            stop,
            standin: Standin::Nothing,
            reload: removes_files,
        };
    };
    let phase = seen.run.phase;
    let process = matches!(phase, RunPhase::InStartJob | RunPhase::Running);
    let standin = if leave && known.holding == Holding::Job && phase == RunPhase::InStartJob {
        Standin::Write {
            kill_signal: seen.run.kill_signal.clone(),
            stop_timeout: seen.run.stop_timeout.clone(),
        }
    } else if ours && (!leave || !process) {
        Standin::Remove {
            clearing: if seen.run.failed {
                Clearing::Needed
            } else {
                Clearing::Not
            },
        }
    } else {
        Standin::Nothing
    };
    Plan {
        stop,
        reload: removes_files || matches!(standin, Standin::Remove { .. }) || seen.held_gone,
        standin,
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;
