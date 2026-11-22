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
//!    id nothing is installed at, whose unit may be somebody else's. Where goetia holds the job the installed
//!    file outranks a stand-in of ours, which is stale: it is rewritten when this call writes one, and otherwise
//!    removed, in every state of the run and under both choices, with no clearing of its own.
//! 4. Where the stand-in is the unit's definition (the sweep of an absent id, a timer alone, `install`), one of
//!    ours that is not written goes: after the stop and the clearing of rule 2 for `Stop`; for `Leave`, when the
//!    one read says the run has no process, after `reset-failed` if it failed. It stays while a run left by
//!    `Leave` has a process.
//! 5. A file at the stand-in's path that is not ours refuses `Leave` of a job goetia removes, before anything
//!    is changed ([`Known::blocked`]): a stand-in could be written there. For a timer alone, with `Stop` and
//!    where nothing is installed it is left alone.
//! 6. Where nothing is installed, and a manager can be asked, a definition systemd holds from the id's own
//!    path whose file is gone is reloaded, under either choice, and nothing else is done for it.
//! 7. `install` applies rule 4 to a stand-in it finds, and nothing else of this.
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
    /// `install`, which settles a stand-in it finds as [`RunInFlight::Leave`] does.
    Install,
}

impl Call {
    /// The call removes the files of an installed id.
    fn removes_files(self) -> bool {
        matches!(self, Call::Job | Call::TimerAlone)
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
    /// Remove it. `reset_failed`: the one read, made with no stop, says the run failed, and the verb refuses a
    /// unit that is not in memory, as one that did not fail is not. Under a stop it is false: what the stop
    /// left is read after it ([`Plan::clears_after_stop`]).
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
}

impl Plan {
    /// After the stop, read the unit once and `reset-failed` it if the run failed (rule 2): a stop that removes
    /// a stand-in which is the unit's definition, `call` being any but a held job.
    pub(in crate::backend::systemd::manager) fn clears_after_stop(&self, call: Call) -> bool {
        self.stop && call != Call::Job && matches!(self.standin, Standin::Remove { .. })
    }
}

impl Known<'_> {
    /// Rule 5: this call is refused before anything is changed.
    pub(in crate::backend::systemd::manager) fn blocked(&self) -> bool {
        self.in_flight == RunInFlight::Leave && self.call == Call::Job && matches!(self.file, StandinFile::NotOurs)
    }

    /// Whether the call reads the job's unit before any stop: `Leave` over an installed id or a stand-in of
    /// ours to settle, and the sweep, which looks for a held definition.
    pub(in crate::backend::systemd::manager) fn reads(&self) -> bool {
        (self.in_flight == RunInFlight::Leave && (self.call.removes_files() || self.file.is_ours()))
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
        // Stale under a held job; under `Stop` the stand-in is the definition, and the read after the stop
        // decides the clearing ([`Plan::clears_after_stop`]), not this one.
        _ if ours && (held_job || !leave) => Standin::Remove { reset_failed: false },
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
    Plan { stop, standin, reload }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;
