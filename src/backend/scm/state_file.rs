//! The per-service state file `goetia-shim` writes and `goetia daemon stop`/`start` read, for
//! `type: simple` services: `%ProgramData%\Goetia\state\<id>`.
//!
//! SCM's status misleads about two things the CLI must know, and the file carries both:
//!
//! - **Whether SCM has a restart queued.** SCM queues recovery for a `STOPPED` with a non-zero
//!   Win32 code and cancels nothing, but its status keeps no trace of when, or of whether a
//!   later start hid it (59-probes.md rounds 2-6). The shim writes a [`FailureRecord`] just
//!   before it reports such a `STOPPED`, and [`ShimState::restart_pending`] is the one rule that
//!   reads it.
//! - **Whether a daemon the shim could not kill is still running.** The shim writes a survivor
//!   (a [`cosca::identity::ProcessIdRecord`]) before it reports the incomplete stop, and a start
//!   is refused only while that process lives.
//!
//! One writer at a time (the shim of the running service) and the file is replaced whole by
//! rename, so a reader sees the old record or the new one. The clock is boot-relative
//! milliseconds plus a boot identity ([`Clock`]), so a record from an earlier boot reads as
//! stale and a wall-clock change does not matter. A missing file is "no record": a service
//! registered before the file existed has none until it is installed again.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cosca::error::{Error as CoscaError, RecordErrorKind};
use cosca::identity::{Liveness, ProcessId, ProcessIdRecord};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
pub mod host;

/// The format version this build writes and the only one it reads.
pub const VERSION: u32 = 1;

/// How long after a restart's due time SCM is still taken to be firing it. The status moves
/// 2.5 to 10.7 ms after the due time in every measured run (59-probes.md, P14), and the clock
/// ticks every 10 to 16 ms; the margin only has to cover both.
pub const FIRE_MARGIN_MS: u64 = 1000;

// Clock ===============================================================================================================

/// Boot-relative milliseconds and the identity of the boot they count from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// `None` when the boot identity could not be read.
    pub boot: Option<u64>,
    pub ms: u64,
}

// Records =============================================================================================================

/// How long after a failure SCM's restart is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ms", rename_all = "snake_case")]
pub enum RestartDue {
    /// The failure queued no restart: no restart action, or the flag that makes SCM act on a
    /// non-crash failure was off. Actions added later queue nothing for it.
    Never,
    /// A restart is due this many milliseconds after the failure.
    After(u64),
    /// The shim could not read the failure actions: a restart may be queued, and for how long
    /// is unknown.
    Unknown,
}

/// A failure the shim reported to SCM with a non-zero Win32 code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub boot: Option<u64>,
    pub at_ms: u64,
    pub restart: RestartDue,
}

impl FailureRecord {
    /// A failure happening now.
    pub fn at(now: Clock, restart: RestartDue) -> Self {
        Self {
            boot: now.boot,
            at_ms: now.ms,
            restart,
        }
    }

    /// Whether `now` is on the boot this was written on. With either identity unreadable it falls
    /// back on the clock: a tick count smaller than the record's means the counter started over.
    fn same_boot(&self, now: Clock) -> bool {
        match (self.boot, now.boot) {
            (Some(recorded), Some(current)) => recorded == current && self.at_ms <= now.ms,
            _ => self.at_ms <= now.ms,
        }
    }

    fn due_at(&self, after_ms: u64) -> u64 {
        self.at_ms.saturating_add(after_ms)
    }

    fn pending(&self, now: Clock, stopped_failed: bool) -> bool {
        if !self.same_boot(now) {
            return false;
        }
        match self.restart {
            RestartDue::Never => false,
            RestartDue::Unknown => true,
            RestartDue::After(after_ms) => {
                now.ms < self.due_at(after_ms).saturating_add(FIRE_MARGIN_MS) || stopped_failed
            }
        }
    }

    /// Whether the restart this record stands for has fired or expired: the entry SCM queued is
    /// spent once a run starts at or after its due time (SCM cannot fire earlier than the failure
    /// plus the delay), and a record that names no due time is spent by any start.
    fn spent_by_a_start_at(&self, now: Clock) -> bool {
        match self.restart {
            RestartDue::After(after_ms) if self.same_boot(now) => now.ms >= self.due_at(after_ms),
            _ => true,
        }
    }
}

/// The restart a failure queued, from the failure-actions configuration it was read from.
///
/// SCM acts on a non-zero Win32 code that is not a crash only when the failure-actions flag is
/// set, and the shim reports every failure that way. `restart_delays` are the delays of the
/// configuration's restart actions; the longest bounds the delay of whichever one a failure
/// count selects.
pub fn restart_due_from(flag: bool, restart_delays: &[Duration]) -> RestartDue {
    match restart_delays.iter().max() {
        Some(longest) if flag => RestartDue::After(u64::try_from(longest.as_millis()).unwrap_or(u64::MAX)),
        _ => RestartDue::Never,
    }
}

/// The whole content of a state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRecord {
    pub v: u32,
    #[serde(default)]
    pub failure: Option<FailureRecord>,
    /// The daemon the last incomplete stop could not kill.
    #[serde(default)]
    pub survivor: Option<ProcessIdRecord>,
}

impl StateRecord {
    pub fn empty() -> Self {
        Self {
            v: VERSION,
            failure: None,
            survivor: None,
        }
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a state record always serializes")
    }

    /// Nothing but whitespace is the empty record, so a file created at install and not yet
    /// written is a valid one. A version other than [`VERSION`] does not decode.
    pub fn decode(text: &str) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Ok(Self::empty());
        }
        let record: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if record.v != VERSION {
            return Err(format!(
                "state file version {} is not the version {VERSION} this goetia reads",
                record.v
            ));
        }
        Ok(record)
    }

    /// What a run starting at `now` makes obsolete in the failure record. `true` if it changed.
    pub fn after_start(&mut self, now: Clock) -> bool {
        if self.failure.as_ref().is_some_and(|f| f.spent_by_a_start_at(now)) {
            self.failure = None;
            return true;
        }
        false
    }
}

// Reading the rule ====================================================================================================

/// What a state file held when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// No file: no record.
    Absent,
    Record(StateRecord),
    /// A file that does not decode, and why.
    Unreadable(String),
}

/// Everything the pending-restart rule reads, taken at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimState {
    pub loaded: Loaded,
    pub now: Clock,
    /// The service's start type is `Disabled`: SCM's restart attempt fails and changes nothing.
    pub disabled: bool,
}

impl ShimState {
    /// Whether SCM has a restart queued that a stop would not cancel, or one a stop would let
    /// fire. `stopped_failed`: the service is `STOPPED` with a non-zero Win32 code.
    ///
    /// Pending iff a failure record exists for this boot and either now is before its due time
    /// plus [`FIRE_MARGIN_MS`], or the service is still stopped with a failure after that (SCM
    /// has not fired yet). A record that names no delay, or cannot be read, counts as pending.
    /// A `Disabled` service has nothing pending. The rule reads the record and never the current
    /// failure actions: a restart queued under other actions fires all the same.
    pub fn restart_pending(&self, stopped_failed: bool) -> bool {
        if self.disabled {
            return false;
        }
        match &self.loaded {
            Loaded::Absent => false,
            Loaded::Unreadable(_) => true,
            Loaded::Record(record) => record
                .failure
                .as_ref()
                .is_some_and(|failure| failure.pending(self.now, stopped_failed)),
        }
    }

    /// Whether the file records a survivor, running or not. Without one, the status record of an
    /// incomplete stop is all there is to say that a daemon may still be running.
    pub fn names_a_survivor(&self) -> bool {
        matches!(&self.loaded, Loaded::Record(record) if record.survivor.is_some())
    }
}

// The file ============================================================================================================

/// The name a record is written under before it replaces `path`. `~` is not in an id's alphabet,
/// so it can never be another daemon's file.
pub fn temporary(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push("~tmp");
    path.with_file_name(name)
}

/// Read the state file at `path`. A missing file is [`Loaded::Absent`]; other I/O failures are
/// errors, and a file that does not decode is [`Loaded::Unreadable`].
pub fn read(path: &Path) -> io::Result<Loaded> {
    let text = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Absent),
        Err(e) => return Err(e),
    };
    Ok(match String::from_utf8(text) {
        Ok(text) => match StateRecord::decode(&text) {
            Ok(record) => Loaded::Record(record),
            Err(why) => Loaded::Unreadable(why),
        },
        Err(e) => Loaded::Unreadable(e.to_string()),
    })
}

/// Replace the state file at `path` with `record`, whole: written beside it and renamed over it.
pub fn write(path: &Path, record: &StateRecord) -> io::Result<()> {
    let temp = temporary(path);
    let written = std::fs::write(&temp, record.encode()).and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        // Best effort: the error being returned is the one that matters.
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Delete the state file at `path` and any temporary left beside it. Neither existing is fine.
pub fn remove(path: &Path) -> io::Result<()> {
    for file in [path.to_path_buf(), temporary(path)] {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Remove the state file of a service going away, except what must outlive it: while a recorded
/// survivor is still running (or cannot be told not to be), only the failure is dropped, so a
/// service registered again under the same id cannot start a second daemon beside it.
pub fn release(path: &Path) -> io::Result<()> {
    let loaded = read(path)?;
    if blocking_survivor(&loaded).is_none() {
        return remove(path);
    }
    let Loaded::Record(mut record) = loaded else {
        unreachable!("only a decoded record names a survivor");
    };
    record.failure = None;
    write(path, &record)
}

/// Run `act`, which ends the shim as a failure SCM will recover from, with `failure` already on
/// disk, as the shim would have left it; a record that cannot be written is an error and `act`
/// does not run. The record is kept if `took_effect` says the act did
/// what it was for; otherwise (the act failed, or the process was already going) the previous
/// record is put back. A service with no state file has no record to write and none is created.
pub fn noting_failure_around<T>(
    path: &Path,
    failure: FailureRecord,
    act: impl FnOnce() -> io::Result<T>,
    took_effect: impl FnOnce(&T) -> bool,
) -> io::Result<T> {
    let not_recorded = |e: io::Error| {
        io::Error::new(
            e.kind(),
            format!("could not record the failure in {}: {e}", path.display()),
        )
    };
    let previous = match read(path).map_err(not_recorded)? {
        Loaded::Absent => return act(),
        Loaded::Record(record) => record,
        // The shim resets a file it cannot read in the same way when it next starts.
        Loaded::Unreadable(_) => StateRecord::empty(),
    };
    let mut noted = previous.clone();
    noted.failure = Some(failure);
    write(path, &noted).map_err(not_recorded)?;
    let result = act();
    if !matches!(&result, Ok(done) if took_effect(done)) {
        // The act's own result is what the caller needs; a record that cannot be put back is
        // spent by the next start like any other.
        let _ = write(path, &previous);
    }
    result
}

/// The record at `path`, or an empty one where there is none or it does not decode.
fn current(path: &Path) -> io::Result<StateRecord> {
    Ok(match read(path)? {
        Loaded::Record(record) => record,
        Loaded::Absent | Loaded::Unreadable(_) => StateRecord::empty(),
    })
}

/// Record `failure`, replacing the previous failure and keeping the survivor. Called just before
/// the shim reports a `STOPPED` with a non-zero Win32 code.
pub fn note_failure(path: &Path, failure: FailureRecord) -> io::Result<()> {
    let mut record = current(path)?;
    record.failure = Some(failure);
    write(path, &record)
}

/// Record the daemon an incomplete stop could not kill. Called before the shim reports it.
pub fn note_survivor(path: &Path, survivor: &ProcessIdRecord) -> io::Result<()> {
    let mut record = current(path)?;
    record.survivor = Some(survivor.clone());
    write(path, &record)
}

// A run starting ======================================================================================================

/// What the shim does with the state file as it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begin {
    Proceed,
    /// The file did not decode and was replaced by an empty record; carries why.
    Reset(String),
    /// A daemon an earlier incomplete stop could not kill is still running (or cannot be told
    /// not to be): launching another would run two.
    Blocked {
        pid: u32,
        survivor: Survivor,
    },
}

/// Settle the file as a run starts: refuse over a live survivor, forget a dead one, and drop a
/// failure the restart this run may be has spent. The file is written only if that changed it,
/// and never created.
pub fn begin_run(path: &Path, now: Clock) -> io::Result<Begin> {
    let mut record = match read(path)? {
        Loaded::Absent => return Ok(Begin::Proceed),
        Loaded::Unreadable(why) => {
            write(path, &StateRecord::empty())?;
            return Ok(Begin::Reset(why));
        }
        Loaded::Record(record) => record,
    };
    let mut changed = false;
    if let Some(survivor) = &record.survivor {
        match survivor_of(survivor) {
            Survivor::Gone => {
                record.survivor = None;
                changed = true;
            }
            alive => {
                return Ok(Begin::Blocked {
                    pid: survivor.pid,
                    survivor: alive,
                });
            }
        }
    }
    changed |= record.after_start(now);
    if changed {
        write(path, &record)?;
    }
    Ok(Begin::Proceed)
}

// Survivors ===========================================================================================================

/// Whether a recorded survivor is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Survivor {
    Alive,
    /// The OS refused the question: it may well be running.
    Unknown,
    Gone,
}

/// Whether the process `record` names is still running. A record that cannot name a process on
/// this host (another platform, another boot) names nothing here; a pid reused by another
/// process is told apart by its creation time.
pub fn survivor_of(record: &ProcessIdRecord) -> Survivor {
    match ProcessId::try_from(record) {
        Ok(id) => match id.is_alive() {
            Liveness::Alive => Survivor::Alive,
            Liveness::Dead => Survivor::Gone,
            Liveness::Unknown => Survivor::Unknown,
        },
        Err(CoscaError::IdentityRecord {
            kind: RecordErrorKind::ScopeUnreadable,
            ..
        }) => Survivor::Unknown,
        Err(_) => Survivor::Gone,
    }
}

/// The survivor that stands in the way of a start: its pid, and whether it is known to be
/// running or only cannot be told not to be.
pub fn blocking_survivor(loaded: &Loaded) -> Option<(u32, Survivor)> {
    let Loaded::Record(record) = loaded else {
        return None;
    };
    let survivor = record.survivor.as_ref()?;
    match survivor_of(survivor) {
        Survivor::Gone => None,
        alive => Some((survivor.pid, alive)),
    }
}

#[cfg(test)]
#[path = "state_file_tests.rs"]
mod state_file_tests;
