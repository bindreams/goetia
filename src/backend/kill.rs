//! `kill` of a service on every backend: one state machine, three targets. It ends the process the
//! manager reports as the service's, and returns once that process has exited and the manager no
//! longer reports it.
//!
//! What makes it safe is a **held** process: the process is opened and kept, the manager is asked
//! **again** whether it is still the service's, and only then is it signalled, through that same hold.
//! While the hold is open the process's identity cannot be taken over by a stranger that got its pid
//! (a handle on Windows, a pidfd on Linux, an audit token on macOS), so a pid the manager confirms
//! after the open is the process held.
//!
//! There is no loop. The process `kill` ends is the one it confirmed as the service's: if that process
//! ended on its own in the moment before the signal, the kill returns once the manager has recorded
//! the end, and the restart policy decides what follows. A caller that wants the next process calls
//! `start`.
//!
//! Targets: `scm::kill`, systemd's and launchd's.
#![cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]

use std::io;
use std::time::Duration;

use crate::error::{
    Error, Pieces, target_failed, terminating_failed, unread_after_open_refused, within, within_delivered,
};
use crate::manager::budget::{Budget, Deadline};

#[cfg(unix)]
pub(crate) mod process;

/// Whether a wait reached the state it was waiting for, or ran out of
/// [`Deadline`] first. An expiry is not an error: the request was issued and
/// the service may yet arrive, so what to say about it is the caller's.
///
/// `#[must_use]` because dropping this reads an expiry as success, and
/// `Result`'s own `#[must_use]` does not catch it: `?` consumes the
/// `Result` and leaves a bare `Waited` behind.
#[must_use = "an expired wait did not confirm the state; decide what to report rather than discarding it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    Confirmed,
    Expired,
}

/// How a kill ended, when the manager could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// The service's process has ended and the manager no longer reports it: by the signal, or on its
    /// own before the signal landed. The restart policy decides what follows either.
    Ended,
    /// The manager reported the service with no process at the first read.
    NotRunning,
}

/// The failure of a kill of `id`, as the verb reports it: a [`Step`](Error::Step) around the error the
/// failure carries, or an OS failure of the kill. A target that talks to a manager through calls that fail
/// with goetia's own [`Error`] wraps it in an [`io::Error`] to pass it through [`KillTarget`]; it is the
/// step's source, with its class, so a caller matching on `NoManager` or `Command` finds it with
/// [`Error::cause`]. A failure after the signal was delivered ([`signalled_but`]) is told under the kill
/// with no claim that the kill failed. Any other error is an OS failure of the kill.
pub(crate) fn kill_failed(id: &str, e: io::Error) -> Error {
    target_failed(format!("kill `{id}`"), e)
}

/// What a [`KillTarget`] needs of the world, so the order of the steps is testable without one.
pub trait KillTarget {
    /// An opened process. Holding it keeps its identity from being taken over; dropping it lets go.
    type Process;

    /// The pid the manager reports for the service's process, in whatever state, or `None` when it
    /// reports none.
    fn service_pid(&mut self) -> io::Result<Option<u32>>;

    /// Open the process holding `pid`; `None` if there is none.
    fn open(&mut self, pid: u32) -> io::Result<Option<Self::Process>>;

    /// Signal `process`, through the hold and never by pid, and do not wait for it. A process that
    /// has exited or is exiting is success: [`Self::await_exit`] is the wait.
    fn terminate(&mut self, process: &Self::Process) -> io::Result<()>;

    /// Whether the manager still vouches for `pid` as the service's process, read after `pid` was
    /// opened as `process`: the second read of the hold. The default is [`Self::service_pid`] being
    /// `pid` again; a manager that can show a pid after its process was reaped adds what ties the
    /// process held to the service, not only its number.
    ///
    /// `false` is the process held not being the service's, because it has exited, the manager has
    /// moved off it, or it is a stranger that was given the number: nothing is signalled.
    fn confirm(&mut self, pid: u32, _process: &Self::Process) -> io::Result<bool> {
        Ok(self.service_pid()? == Some(pid))
    }

    /// Block until `process` has exited. Ends at `deadline` as [`Waited::Expired`], with the process
    /// running.
    fn await_exit(&mut self, process: &Self::Process, deadline: Deadline) -> io::Result<Waited>;

    /// Say `line` where the person running the verb reads it. The state machine says why a wait that
    /// lasts blocks; a target that has somewhere else to say it overrides this, and a test that wants
    /// to see it does too.
    fn notice(&mut self, line: &str) {
        eprintln!("{line}");
    }

    /// Block until the manager no longer reports `pid` as the service's process: it reports none, or
    /// another pid, or no longer has the service at all. Ends at `deadline` as [`Waited::Expired`].
    ///
    /// `held` is the process `pid` was held through, `None` when it was gone before it could be
    /// opened. The same `pid` on another process is the service's new process when the manager says so,
    /// and a stranger's number over a stale report when it does not (launchd's `print` after the reap).
    /// A target whose held handle keeps the pid from being reused (SCM's) has no such case.
    ///
    /// The manager records an exit after it happens, so a read right after can still show the pid.
    fn await_recorded(&mut self, pid: u32, held: Option<&Self::Process>, deadline: Deadline) -> io::Result<Waited>;
}

/// What an unbounded wait for `id`'s process `pid` says it is blocked on. `terminate_failed` is why
/// the process is still running when ending it failed. Text on purpose: a line for stderr, with no
/// error under it that anything could search or take a remedy from.
pub(crate) fn waiting_notice(id: &str, pid: u32, terminate_failed: Option<&io::Error>) -> String {
    terminating_failed(
        Pieces::new().text(format!("{id}: waiting for process {pid} to exit")),
        terminate_failed,
        "",
    )
    .to_string()
}

/// How long a kill waits before saying why it is still waiting. Cosmetic: the wait does not end at it.
const NOTICE_AFTER: Duration = Duration::from_secs(1);

/// What an unbounded wait for the manager's record of `id`'s process `pid` says it is blocked on.
fn recording_notice(id: &str, pid: u32) -> String {
    format!("{id}: {}", recording_wait(pid))
}

/// What the wait for the manager's record of `pid` is, in words true whether the process exited or the
/// manager moved off it.
fn recording_wait(pid: u32) -> String {
    format!("waiting for the manager to stop reporting process {pid}")
}

/// `wait` under [`NOTICE_AFTER`], and once it has lasted that long `line` said and `wait` again with no
/// bound: a wait that blocks would otherwise leave the verb looking stuck. The exit and the record are
/// the events, so the second wait ends on them.
pub(crate) fn wait_saying<T: KillTarget>(
    target: &mut T,
    line: &str,
    mut wait: impl FnMut(&mut T, Deadline) -> io::Result<Waited>,
) -> io::Result<()> {
    if wait(target, Budget::Bounded(NOTICE_AFTER).start())? == Waited::Confirmed {
        return Ok(());
    }
    wait_unbounded_saying(target, line, wait)
}

/// `line` said, and `wait` with no bound.
fn wait_unbounded_saying<T: KillTarget>(
    target: &mut T,
    line: &str,
    mut wait: impl FnMut(&mut T, Deadline) -> io::Result<Waited>,
) -> io::Result<()> {
    target.notice(line);
    match wait(target, Budget::Unbounded.start())? {
        Waited::Confirmed => Ok(()),
        Waited::Expired => unreachable!("an unbounded deadline never expires"),
    }
}

/// End the service's process, or report that it has none. See the module doc for what it returns on.
/// `id` is the service's, for the line said before a wait that blocks.
///
/// The signal is sent before any wait, so a caller that dies in the wait still has the process ended.
pub fn kill_service<T: KillTarget>(target: &mut T, id: &str) -> io::Result<KillOutcome> {
    kill_service_naming(target, id, |e| e)
}

/// [`kill_service`], where the failure of the first read of the manager is told by `unread`: a caller that
/// kills one of several processes of `id` says which in it.
pub(crate) fn kill_service_naming<T: KillTarget>(
    target: &mut T,
    id: &str,
    unread: impl FnOnce(io::Error) -> io::Error,
) -> io::Result<KillOutcome> {
    match hold_once(target, Budget::Bounded(NOTICE_AFTER).start()) {
        // The hold's wait for the record of a process gone before it could be opened is the kill's own.
        Err(HoldError::Expired { pid }) => {
            wait_unbounded_saying(target, &recording_notice(id, pid), |t, d| {
                t.await_recorded(pid, None, d)
            })
            .map_err(|e| of_process(pid, Step::Recording, e))?;
            Ok(KillOutcome::Ended)
        }
        Err(HoldError::Unread(e)) => Err(unread(e)),
        Err(HoldError::Refused(e) | HoldError::Read(e)) => Err(e),
        Ok(Hold::NoProcess) => Ok(KillOutcome::NotRunning),
        // The process held is not the service's, or was gone: it is not signalled, and a manager may
        // still report its pid for a moment, so the kill waits for the record as after a signal.
        Ok(Hold::Ended { pid, held }) => {
            wait_saying(target, &recording_notice(id, pid), |t, d| {
                t.await_recorded(pid, held.as_ref(), d)
            })
            .map_err(|e| of_process(pid, Step::Recording, e))?;
            Ok(KillOutcome::Ended)
        }
        Ok(Hold::Held { pid, process }) => {
            target.terminate(&process).map_err(|e| not_signalled(pid, e))?;
            wait_saying(target, &waiting_notice(id, pid, None), |t, d| t.await_exit(&process, d))
                .map_err(|e| signalled_but(pid, e))?;
            wait_saying(target, &recording_notice(id, pid), |t, d| {
                t.await_recorded(pid, Some(&process), d)
            })
            .map_err(|e| signalled_but(pid, e))?;
            Ok(KillOutcome::Ended)
        }
    }
}

/// What one look at the daemon's process came to.
#[derive(Debug)]
pub(crate) enum Hold<P> {
    /// The manager reports the daemon with no process.
    NoProcess,
    /// The daemon's process, opened, and confirmed by a second read of the manager.
    Held { pid: u32, process: P },
    /// The process `pid` the manager reported is not the daemon's: it was gone before it could be
    /// opened, the manager moved off the pid between the open and the confirmation, or the process
    /// opened (`held`) is not the daemon's. Nothing is signalled. The manager may still report `pid`
    /// for a moment, so a caller who needs its record waits with [`KillTarget::await_recorded`] over
    /// `pid` and `held`.
    Ended { pid: u32, held: Option<P> },
}

/// The daemon's process and its pid, opened and held, once: what everything that acts on a daemon's
/// process starts from.
///
/// A process gone before the open is waited for in the manager until `deadline`, which ends as
/// [`HoldError::Expired`]; that is the hold's only wait. A daemon that moved on is [`Hold::Ended`].
pub(crate) fn hold_once<T: KillTarget>(
    target: &mut T,
    deadline: Deadline,
) -> std::result::Result<Hold<T::Process>, HoldError> {
    let Some(pid) = target.service_pid().map_err(HoldError::Unread)? else {
        return Ok(Hold::NoProcess);
    };
    let process = match target.open(pid) {
        Ok(Some(process)) => process,
        // The process is gone and the manager has not yet recorded it.
        Ok(None) => {
            let waited = target
                .await_recorded(pid, None, deadline)
                .map_err(|e| HoldError::Read(of_process(pid, Step::Recording, e)))?;
            return match waited {
                Waited::Confirmed => Ok(Hold::Ended { pid, held: None }),
                Waited::Expired => Err(HoldError::Expired { pid }),
            };
        }
        // A pid that cannot be opened is a refusal only while the manager still says it is the daemon's.
        Err(e) => {
            return match target.service_pid() {
                Ok(Some(now)) if now == pid => Err(HoldError::Refused(of_process(pid, Step::Opening, e))),
                // The manager has moved on: the pid was a process that exited, or somebody else's,
                // and `e` is superseded by that.
                Ok(_) => Ok(Hold::Ended { pid, held: None }),
                // Cannot tell a refusal from a stale pid; both errors are the caller's to see.
                Err(read) => Err(HoldError::Read(unread_after_open_refused(pid, e, read))),
            };
        }
    };
    let confirmed = target
        .confirm(pid, &process)
        .map_err(|e| HoldError::Read(of_process(pid, Step::Confirming, e)))?;
    if confirmed {
        Ok(Hold::Held { pid, process })
    } else {
        Ok(Hold::Ended {
            pid,
            held: Some(process),
        })
    }
}

/// A step of holding the daemon's process, as a failure of it says so. The record wait says what
/// [`recording_notice`] says of it: a process the manager moved off is not known to be gone.
#[derive(Clone, Copy)]
enum Step {
    Opening,
    Confirming,
    Recording,
}

/// `e`, a failure of `step` on the process `pid` the manager named, said within a context that names the
/// process: the report of every failure of a hold after the pid is known says which process it was. What
/// the backend's own text says of the pid is its own.
fn of_process(pid: u32, step: Step, e: io::Error) -> io::Error {
    within(
        match step {
            Step::Opening => format!("opening process {pid}"),
            Step::Confirming => format!("confirming that process {pid} is the daemon's"),
            Step::Recording => recording_wait(pid),
        },
        e,
    )
}

/// Why [`hold_once`] could not hold the daemon's process. The OS code of the error in a variant is read
/// with `os_error_in`, which [`Error::raw_os_error`] searches with, and never with
/// `io::Error::raw_os_error`: the error is said within contexts, or is a pair of failures, which only
/// that search sees through.
#[derive(Debug)]
pub(crate) enum HoldError {
    /// The manager could not be read for the process at all, so none is known: a caller that already
    /// knows which it is says so.
    Unread(io::Error),
    /// The manager vouches for the pid, and the process cannot be opened. The error says which process.
    Refused(io::Error),
    /// The manager could not be read after it named a pid, so what the daemon runs is not known. A read
    /// made after the manager named a pid says which process it was about; the failure of the read that
    /// follows a refused open says it itself.
    Read(io::Error),
    /// The manager still reported `pid`, whose process is gone, when the deadline ran out.
    Expired { pid: u32 },
}

impl From<HoldError> for io::Error {
    fn from(error: HoldError) -> Self {
        match error {
            HoldError::Unread(e) | HoldError::Refused(e) | HoldError::Read(e) => e,
            HoldError::Expired { pid } => io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the manager still reported process {pid} as the daemon's when the wait ended"),
            ),
        }
    }
}

/// `e`, an error from the waits after the signal to `pid` was delivered: the error must not read as a
/// failed kill.
fn signalled_but(pid: u32, e: io::Error) -> io::Error {
    within_delivered(
        format!("process {pid} was signalled, but waiting for it to exit and for the manager to record that failed"),
        e,
    )
}

/// `e`, the failure to signal the process `pid`, which was held and confirmed as the daemon's: the
/// signal was not delivered, and the error says which process it was.
fn not_signalled(pid: u32, e: io::Error) -> io::Error {
    within(
        format!("process {pid} was held and confirmed as the daemon's, but signalling it failed"),
        e,
    )
}

#[cfg(test)]
#[path = "kill_tests.rs"]
mod kill_tests;
