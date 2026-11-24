//! What `uninstall` and `install` do about a run in flight and a stand-in, decided here and nowhere else.
//!
//! [`plan`] takes everything known and returns everything to do; `Systemd::settle` reads the facts, calls it
//! and carries the plan out in one order. The rules, each written once:
//!
//! 1. [`RunInFlight::Leave`] never stops: a stop sent after a read of a restart that is waiting could land on a
//!    process systemd started in between.
//! 2. [`RunInFlight::Stop`] stops when goetia holds a job or a stand-in of its own is there, and never
//!    otherwise. Nothing is read before the stop for this; the sweep of an absent id reads first for its own
//!    sake (rule 6). Where the stand-in is the unit's definition (rule 4) and is removed, the unit is read once
//!    after the stop, and a run that read `failed` (a stopped oneshot does) is cleared with `reset-failed` before
//!    the files go ([`Plan::clears_after_stop`]).
//! 3. A stand-in is written (or one of ours replaced) only for `Leave`, a job goetia removes, and a run still
//!    in its start job: with the two values the read printed. A restart that is waiting
//!    ([`RunPhase::PendingRestart`]) is not such a run, and is left to systemd. Never for a timer alone or an
//!    id nothing is installed at, whose unit may be somebody else's. Where an installed job file is there (an
//!    `uninstall` of a held job, an `install` over one) it outranks a stand-in of ours, which is stale: it is
//!    rewritten when this call writes one, and otherwise removed, in every state of the run and under both
//!    choices, with no clearing of its own. A stand-in this call wrote is not taken back when a removal fails.
//! 4. Where the stand-in is the unit's definition (the sweep of an absent id, a timer alone, an `install` over
//!    no job file), one of ours that is not written goes. `Stop` stops first and clears the run that failed
//!    (rule 2), as a stop precedes the verified removal of the installed units. `Leave` removes it when the one
//!    read says the run has no process, and clears a run that failed with `reset-failed` only once the stand-in
//!    was found still ours and moved aside, since nothing was stopped. It stays while a run left by `Leave` has a
//!    process. A stand-in is removed only if it is still the file read as ours; one that has vanished or
//!    changed is left alone, and the call goes on.
//! 5. A file at the stand-in's path that is not ours refuses `Leave` of a job goetia removes, before anything
//!    is changed ([`Known::blocked`]): a stand-in could be written there. For a timer alone, with `Stop` and
//!    where nothing is installed it is left alone.
//! 6. Where nothing is installed, and a manager can be asked, a definition systemd holds from the id's own
//!    path whose file is gone is reloaded, under either choice, and nothing else is done for it.
//! 7. `install` over an id with no job file applies rule 4 to a stand-in it finds, and over a job file applies
//!    rule 3's removal, which needs no read; it never stops and never writes.
//!
//! Every plan that stops, writes or removes also reloads, so the plan that reloads passes the version gate
//! before its first change; one that does not changes nothing and passes none.

use crate::manager::RunInFlight;

use super::StandinFile;
use crate::backend::systemd::manager::systemctl::{RunPhase, UnitRun};

/// What a call is about: one fact, from which what goetia removes and what it looks for follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend::systemd::manager) enum Call {
    /// `uninstall` of an installed id whose snapshot has the job's own file, which this call removes.
    Job,
    /// `uninstall` of an installed id whose snapshot has no job file: a timer alone.
    TimerAlone,
    /// `uninstall` of an id nothing is installed at: it looks for a definition systemd still holds whose file
    /// is gone (rule 6).
    Sweep,
    /// `install` of an id with no job file: a stand-in of ours there is the unit's definition, settled as
    /// [`RunInFlight::Leave`] does.
    Install,
    /// `install` over an id whose job file is installed: the file outranks a stand-in of ours, which is stale.
    Update,
}

impl Call {
    /// The call removes the files of an installed id.
    fn removes_files(self) -> bool {
        matches!(self, Call::Job | Call::TimerAlone)
    }

    /// An installed job file is there, so a stand-in of ours is stale (rule 3).
    fn outranks_standin(self) -> bool {
        matches!(self, Call::Job | Call::Update)
    }
}

/// What is known before the run is read.
#[derive(Debug)]
pub(in crate::backend::systemd::manager) struct Known<'a> {
    pub in_flight: RunInFlight,
    pub call: Call,
    pub file: &'a StandinFile,
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
    /// Remove it. `reset_failed`: the stand-in is the unit's definition, nothing was stopped and the one read
    /// says the run failed, so the run is cleared once the stand-in is found ours and moved aside, before it is
    /// deleted (the verb refuses a unit that is not in memory, as one that did not fail is not). False for a
    /// stale stand-in, which has no clearing, and under a stop, where the stop and the clearing that follows it
    /// ([`Plan::clears_after_stop`], decided from a read after the stop) come before the removal.
    Remove { reset_failed: bool },
}

/// Everything to do, in the order `Systemd::settle` does it: the stop, the stand-in, the reload.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::backend::systemd::manager) struct Plan {
    /// Stop the run.
    pub stop: bool,
    pub standin: Standin,
    /// The manager is reloaded once, after the files and the stand-in are dealt with.
    pub reload: bool,
    /// After the stop, read the unit once and `reset-failed` it if the run failed (rule 2): a stop of a
    /// stand-in that is the unit's definition, which the removal follows.
    pub clears_after_stop: bool,
}

impl Known<'_> {
    /// Rule 5: this call is refused before anything is changed.
    pub(in crate::backend::systemd::manager) fn blocked(&self) -> bool {
        self.in_flight == RunInFlight::Leave && self.call == Call::Job && matches!(self.file, StandinFile::NotOurs)
    }

    /// Whether the call reads the job's unit before any stop: `Leave` over an installed id or a stand-in of
    /// ours that is the unit's definition, and the sweep, which looks for a held definition. A stale stand-in
    /// (`install` over a job file) needs no read.
    pub(in crate::backend::systemd::manager) fn reads(&self) -> bool {
        (self.in_flight == RunInFlight::Leave
            && (self.call.removes_files() || (self.file.is_ours() && self.call != Call::Update)))
            || self.call == Call::Sweep
    }
}

/// The plan for a call that is not [`Known::blocked`]. `seen` is `Some` exactly when [`Known::reads`].
pub(in crate::backend::systemd::manager) fn plan(known: &Known<'_>, seen: Option<&Seen>) -> Plan {
    debug_assert!(!known.blocked());
    debug_assert_eq!(known.reads(), seen.is_some());
    let leave = known.in_flight == RunInFlight::Leave;
    let held_job = known.call == Call::Job;
    let ours = known.file.is_ours();
    let stop = !leave && (held_job || ours);
    let standin = match seen.map(|seen| &seen.run) {
        Some(run) if leave && held_job && run.phase == RunPhase::InStartJob => Standin::Write {
            kill_signal: run.kill_signal.clone(),
            stop_timeout: run.stop_timeout.clone(),
        },
        // Stale under an installed job file; under `Stop` the stand-in is the definition, and the read after
        // the stop decides the clearing ([`Plan::clears_after_stop`]), not this one.
        _ if ours && (known.call.outranks_standin() || !leave) => Standin::Remove { reset_failed: false },
        Some(run) if ours && !matches!(run.phase, RunPhase::InStartJob | RunPhase::Running) => Standin::Remove {
            reset_failed: run.failed,
        },
        _ => Standin::Nothing,
    };
    let reload = known.call.removes_files()
        || matches!(standin, Standin::Remove { .. })
        || seen.is_some_and(|seen| seen.held_gone);
    debug_assert!(
        reload || !(stop || standin != Standin::Nothing),
        "a plan that stops, writes or removes reloads: the version gate is passed by the reload"
    );
    let clears_after_stop = stop && !known.call.outranks_standin() && matches!(standin, Standin::Remove { .. });
    Plan {
        stop,
        standin,
        reload,
        clears_after_stop,
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;
