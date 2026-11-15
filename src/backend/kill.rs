//! `kill` of a service on every backend: one state machine, three targets. It ends the process the
//! manager reports as the service's, and returns once that process has exited and the manager no
//! longer reports it.
//!
//! What makes it safe is a **held** process: the process is opened and kept, the manager is asked
//! **again** whether it is still the service's, and only then is it signalled, through that same hold.
//! While the hold is open the process's identity cannot be taken over by a stranger that got its pid
//! (a handle on Windows, a pidfd on Linux, an audit token on macOS), so a pid the manager confirms
//! after the open is the process held. That premise is the hold's own; the confirmation is the
//! manager's own read of the service, as `stop` has it.
//!
//! There is no loop. The process `kill` ends is the one it confirmed as the service's: if that process
//! ended on its own in the moment before the signal, the kill returns once the manager has recorded
//! the end, and the restart policy decides what follows, as it would had the process exited on its
//! own a moment before `kill` was called. A caller that wants the next process calls `start`.
//!
//! A pure state machine over [`KillTarget`], unit-tested with a script in `kill_tests.rs`. The
//! targets are SCM's (`scm::kill`), systemd's and launchd's.
#![cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]

use std::io;

use crate::error::{Chain, Error, terminating_failed, within};
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

/// The failure of a kill of `id`, as the verb reports it. A target that talks to a manager through
/// calls that fail with goetia's own [`Error`] wraps it in an [`io::Error`] to pass it through
/// [`KillTarget`]; it comes back as itself, with its kind. Any other error is an OS failure of the kill.
pub(crate) fn kill_failed(id: &str, e: io::Error) -> Error {
    if e.get_ref().is_some_and(|inner| inner.is::<Error>()) {
        let inner = e.into_inner().expect("an error with a source was just seen");
        return *inner.downcast::<Error>().expect("an `Error` was just seen");
    }
    Error::os(format!("kill `{id}`"), e)
}

/// What a [`KillTarget`] needs of the world, so the order of the steps is testable without one.
///
/// `Expired` is returned only after a bounded deadline has passed; an unbounded deadline never
/// expires.
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

    /// Whether `process` has exited: a read of its state, not a wait.
    fn has_exited(&mut self, process: &Self::Process) -> io::Result<bool>;

    /// Block until `process` has exited. Unbounded: the exit is the event.
    fn await_exit(&mut self, process: &Self::Process) -> io::Result<()>;

    /// Say `line` where the person running the verb reads it. The state machine says why an unbounded
    /// wait blocks; a target that has somewhere else to say it overrides this, and a test that wants to
    /// see it does too.
    fn notice(&mut self, line: &str) {
        eprintln!("{line}");
    }

    /// Block until the manager no longer reports `pid` as the service's process: it reports none, or
    /// another pid, or no longer has the service at all. Ends at `deadline` as [`Waited::Expired`].
    ///
    /// `held` is the process `pid` was held through, `None` when it was gone before it could be
    /// opened. A manager that reports `pid` is reporting that same process only while the process is
    /// still there to be reported: once it has been reaped, a `pid` the manager reports is another
    /// process that was given the number, and the end of the held one is recorded. A target whose
    /// held handle keeps the pid from being reused (SCM's) has no such case.
    ///
    /// The manager learns of an exit after it has happened, so a read straight after the exit can
    /// still show the pid; this is the wait for the manager's own record, which is what a caller who
    /// reads the manager next needs.
    fn await_recorded(&mut self, pid: u32, held: Option<&Self::Process>, deadline: Deadline) -> io::Result<Waited>;
}

/// What an unbounded wait for `id`'s process `pid` says it is blocked on. `terminate_failed` is why
/// the process is still running when ending it failed.
pub(crate) fn waiting_notice(id: &str, pid: u32, terminate_failed: Option<&io::Error>) -> String {
    terminating_failed(format!("{id}: waiting for process {pid} to exit"), terminate_failed, "").to_string()
}

/// Wait for `process`, held for `id`, to exit, and say why first when it has not: a process in
/// uninterruptible sleep on a hung device is never signalled, and without the line the verb would look
/// stuck.
fn await_exit_saying<T: KillTarget>(target: &mut T, process: &T::Process, id: &str, pid: u32) -> io::Result<()> {
    if !target.has_exited(process)? {
        target.notice(&waiting_notice(id, pid, None));
    }
    target.await_exit(process)
}

/// End the service's process, or report that it has none. See the module doc for what it returns on.
/// `id` is the service's, for the line said before a wait that blocks.
///
/// The signal is sent before any wait, so a caller that dies in the wait still has the process ended.
pub fn kill_service<T: KillTarget>(target: &mut T, id: &str) -> io::Result<KillOutcome> {
    match hold_once(target, Budget::Unbounded.start()) {
        Err(HoldError::Expired { .. }) => unreachable!("an unbounded deadline never expires"),
        Err(HoldError::Refused(e) | HoldError::Read(e)) => Err(e),
        Ok(Hold::NoProcess) => Ok(KillOutcome::NotRunning),
        // The manager moved off the pid, or let go of one whose process was gone. A held process was
        // then gone too, or a stranger that took its pid: neither is waited for.
        Ok(Hold::Ended) => Ok(KillOutcome::Ended),
        Ok(Hold::Held { pid, process }) => {
            target.terminate(&process)?;
            await_exit_saying(target, &process, id, pid).map_err(|e| signalled_but(pid, e))?;
            match target.await_recorded(pid, Some(&process), Budget::Unbounded.start()) {
                Ok(Waited::Confirmed) => Ok(KillOutcome::Ended),
                Ok(Waited::Expired) => unreachable!("an unbounded deadline never expires"),
                Err(e) => Err(signalled_but(pid, e)),
            }
        }
    }
}

/// What one look at the service's process came to.
#[derive(Debug)]
pub(crate) enum Hold<P> {
    /// The manager reports the service with no process.
    NoProcess,
    /// The service's process, opened, and confirmed by a second read of the manager.
    Held { pid: u32, process: P },
    /// The pid the manager reported is no longer the service's: the process was gone before it could
    /// be opened, and the manager has recorded that, or the manager moved off the pid between the
    /// open and the confirmation. A process opened by then is let go, not waited for: it is gone or
    /// somebody else's.
    Ended,
}

/// The service's process and its pid, opened and held, once: what everything that acts on a service's
/// process starts from (`kill` signals it, `stop` and `uninstall` wait for it).
///
/// The process is read, opened and read again, and the pid the manager still vouches for is the
/// process held. A process gone before the open is waited for in the manager, until `deadline`, which
/// ends as [`HoldError::Expired`]: that wait is the hold's only one. Nothing is re-read to chase a
/// service that moved on; that is [`Hold::Ended`], and what it means is the caller's.
pub(crate) fn hold_once<T: KillTarget>(
    target: &mut T,
    deadline: Deadline,
) -> std::result::Result<Hold<T::Process>, HoldError> {
    let Some(pid) = target.service_pid().map_err(HoldError::Read)? else {
        return Ok(Hold::NoProcess);
    };
    let process = match target.open(pid) {
        Ok(Some(process)) => process,
        // The process is gone and the manager has not yet recorded it.
        Ok(None) => {
            return match target.await_recorded(pid, None, deadline).map_err(HoldError::Read)? {
                Waited::Confirmed => Ok(Hold::Ended),
                Waited::Expired => Err(HoldError::Expired { pid }),
            };
        }
        // A pid that cannot be opened is a refusal only while the manager still says it is the service's.
        Err(e) => {
            return match target.service_pid() {
                Ok(Some(now)) if now == pid => Err(HoldError::Refused(e)),
                // The manager has moved on: the pid was a process that exited, or somebody else's,
                // and `e` is superseded by that.
                Ok(_) => Ok(Hold::Ended),
                // Cannot tell a refusal from a stale pid; both errors are the caller's to see.
                Err(read) => Err(HoldError::Read(io::Error::new(
                    read.kind(),
                    ReadAfterRefusedOpen { read, open: e },
                ))),
            };
        }
    };
    if target.service_pid().map_err(HoldError::Read)? == Some(pid) {
        Ok(Hold::Held { pid, process })
    } else {
        // Let go: the manager has moved off it.
        drop(process);
        Ok(Hold::Ended)
    }
}

/// Why [`hold_once`] could not hold the service's process.
#[derive(Debug)]
pub(crate) enum HoldError {
    /// The manager vouches for the pid, and the process cannot be opened.
    Refused(io::Error),
    /// The manager could not be read, so what the service runs is not known.
    Read(io::Error),
    /// The manager still reported `pid`, whose process is gone, when the deadline ran out.
    Expired { pid: u32 },
}

impl From<HoldError> for io::Error {
    fn from(error: HoldError) -> Self {
        match error {
            HoldError::Refused(e) | HoldError::Read(e) => e,
            HoldError::Expired { pid } => io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the manager still reported process {pid} as the service's when the wait ended"),
            ),
        }
    }
}

/// `e`, an error from the waits after the signal to `pid` was delivered: the process is ending and
/// only the wait for it, or for the manager to record that, failed, so the error must not read as a
/// failed kill and a caller does not retry blindly.
fn signalled_but(pid: u32, e: io::Error) -> io::Error {
    within(
        format!("process {pid} was signalled, but waiting for it to exit and for the manager to record that failed"),
        e,
    )
}

/// The service's pid could not be opened, and the read that would say whether the manager still
/// vouches for it failed too. Both errors are the caller's to see: `read`'s text, then `open`'s in
/// parentheses. Neither is the [`source`](std::error::Error::source), which would print it a second
/// time.
#[derive(Debug, thiserror::Error)]
#[error("{} (after the service's process could not be opened: {})", Chain(read), Chain(open))]
struct ReadAfterRefusedOpen {
    read: io::Error,
    open: io::Error,
}

#[cfg(test)]
#[path = "kill_tests.rs"]
mod kill_tests;
