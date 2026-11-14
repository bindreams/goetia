//! A kqueue that says when a process has exited, and how: what `stop` waits on after it signals a run,
//! and what `run` follows to its end.
//!
//! `NOTE_EXIT | NOTE_EXITSTATUS` on a pid reports its exit with the `wait(2)` status, for a process that
//! is not goetia's child as well, when the caller could signal it (measured on macOS CI, as root). The
//! registration must be made while the pid is alive: a process that has already exited is out of
//! kqueue's reach, and its pid may be reused, so [`Watch::add`] says [`Added::Gone`] for it and the
//! caller reads launchd again.
//!
//! A registration is a value, [`Registered`]: waiting consumes it, and dropping it, on whatever path
//! leaves the pass that made it, deletes the registration and drains the queue. So no exit event
//! outlives its pass, and a later registration of a reused pid number can never be handed the earlier
//! process's. One [`Watch`] serves every pid of one verb in turn, one registration at a time.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

use crate::error::{Error, Result};
use crate::manager::budget::Deadline;

/// One kqueue fd.
pub(crate) struct Watch {
    queue: Arc<Kqueue>,
}

/// What [`Watch::add`] found.
pub(crate) enum Added {
    /// The pid is registered: its exit will be reported by [`Registered::wait`], even one that happens
    /// before the wait starts.
    Watching(Registered),
    /// No such process: it has exited, or never was.
    Gone,
}

/// A pid registered for its exit. It shares the queue, so it outlives the [`Watch`] that made it.
pub(crate) struct Registered {
    queue: Arc<Kqueue>,
    pid: u32,
}

/// The longest a single `kevent` call is asked to block, in seconds. A `wait` under a longer deadline
/// asks again with what is left, so this is a limit of the call and decides nothing.
const LONGEST_CALL_SECS: u64 = i32::MAX as u64;

impl Watch {
    /// Made before a verb sends its first request, so that failing to make it (a process out of file
    /// descriptors) strands nothing.
    pub(crate) fn new() -> Result<Watch> {
        make_queue()
            .map(|queue| Watch { queue: Arc::new(queue) })
            .map_err(|source| Error::os("make a kqueue to watch processes exit with", source))
    }

    /// Registers `pid` for its exit and the status it exits with. [`Added::Gone`] where there is no such
    /// process. One registration at a time: the previous one is waited on or dropped first.
    pub(crate) fn add(&self, pid: u32) -> Result<Added> {
        let change = KEvent::new(
            pid as usize,
            EventFilter::EVFILT_PROC,
            // A receipt, so that the answer for this one change comes back in `receipt`, whatever the
            // queue already holds.
            EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_RECEIPT,
            FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
            0,
            0,
        );
        let mut receipt = [blank()];
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let answered = loop {
            match self.queue.kevent(&[change], &mut receipt, Some(now)) {
                Err(Errno::EINTR) => {}
                Err(Errno::ESRCH) => return Ok(Added::Gone),
                other => break other,
            }
        };
        let watching = |source: io::Error| Error::os(format!("watch process {pid} exit"), source);
        let answered = answered.map_err(|errno| watching(errno.into()))?;
        debug_assert_eq!(answered, 1, "EV_RECEIPT answers each change");
        let [receipt] = receipt;
        debug_assert!(
            receipt.flags().contains(EvFlags::EV_ERROR),
            "a receipt is an EV_ERROR event"
        );
        match receipt.data() {
            0 => Ok(Added::Watching(Registered {
                queue: Arc::clone(&self.queue),
                pid,
            })),
            code => match Errno::from_raw(code as i32) {
                Errno::ESRCH => Ok(Added::Gone),
                errno => Err(watching(errno.into())),
            },
        }
    }
}

impl Registered {
    /// Blocks until the pid has exited, and answers its raw `wait(2)` status. `None` iff `deadline`
    /// expired first, and none expires without one. An exit that happened before the wait began is
    /// reported at once.
    pub(crate) fn wait(self, deadline: Option<Deadline>) -> Result<Option<i32>> {
        let pid = self.pid;
        let mut events = [blank(); 8];
        loop {
            // Asked again with what is left each time: the kernel's clock is not the deadline's, and
            // a call that returns early finds the deadline not yet spent.
            let remaining = deadline.and_then(|deadline| deadline.remaining());
            let timeout = remaining.map(timespec);
            match self.queue.kevent(&[], &mut events, timeout) {
                Ok(0) => {
                    if deadline.is_some_and(|deadline| deadline.expired()) {
                        return Ok(None);
                    }
                }
                Ok(n) => {
                    debug_assert!(
                        events[..n].iter().all(|event| event.ident() == pid as usize),
                        "the queue holds only the one registration's events"
                    );
                    if let Some(status) = events[..n].iter().find_map(|event| exit_status(event, pid)) {
                        return status.map(Some);
                    }
                }
                Err(Errno::EINTR) => {}
                Err(errno) => {
                    return Err(Error::os(
                        format!("wait for process {pid} to exit"),
                        io::Error::from(errno),
                    ));
                }
            }
        }
    }

    /// Deletes the registration, and answers what the kernel said of it: `Ok` where it was still there
    /// (its event, if it had fired and was not read, goes with it), `ENOENT` where there was none to
    /// delete. [`Drop`] does not depend on the answer.
    fn delete(&self) -> std::result::Result<(), Errno> {
        let change = KEvent::new(
            self.pid as usize,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_DELETE | EvFlags::EV_RECEIPT,
            FilterFlag::empty(),
            0,
            0,
        );
        let mut receipt = [blank()];
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        loop {
            match self.queue.kevent(&[change], &mut receipt, Some(now)) {
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno),
                Ok(0) => return Ok(()),
                Ok(_) => {
                    return match receipt[0].data() {
                        0 => Ok(()),
                        code => Err(Errno::from_raw(code as i32)),
                    };
                }
            }
        }
    }

    /// Reads every event the queue holds, discarding them. Whatever `delete` did or did not remove, no
    /// event of this registration is left.
    fn drain(&self) {
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut events = [blank(); 8];
        loop {
            match self.queue.kevent(&[], &mut events, Some(now)) {
                Ok(0) => return,
                Ok(_) | Err(Errno::EINTR) => {}
                // A queue that cannot be read holds nothing a later wait could be handed.
                Err(_) => return,
            }
        }
    }
}

impl Drop for Registered {
    /// Deletes the registration and drains the queue, answering to nobody: the exit may have been waited
    /// on already (`delete` finds nothing), or fired unread (its event goes with the knote, or is drained).
    fn drop(&mut self) {
        _ = self.delete();
        self.drain();
    }
}

/// `event`'s status if it is `pid`'s exit: `None` for any other event.
fn exit_status(event: &KEvent, pid: u32) -> Option<Result<i32>> {
    let is_exit = event.ident() == pid as usize
        && event.filter() == Ok(EventFilter::EVFILT_PROC)
        && event.fflags().contains(FilterFlag::NOTE_EXIT);
    is_exit.then(|| {
        if event.fflags().contains(FilterFlag::NOTE_EXITSTATUS) {
            Ok(event.data() as i32)
        } else {
            Err(Error::os(
                format!("wait for process {pid} to exit"),
                io::Error::other("the kernel reported its exit without its status"),
            ))
        }
    })
}

fn blank() -> KEvent {
    KEvent::new(0, EventFilter::EVFILT_PROC, EvFlags::empty(), FilterFlag::empty(), 0, 0)
}

fn timespec(remaining: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: remaining.as_secs().min(LONGEST_CALL_SECS) as libc::time_t,
        tv_nsec: remaining.subsec_nanos().into(),
    }
}

fn make_queue() -> io::Result<Kqueue> {
    #[cfg(test)]
    test_hook::make()?;
    Kqueue::new().map_err(io::Error::from)
}

#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;

    use crate::backend::test_allowance::{Allowance, spend};

    thread_local! {
        static QUEUES: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Let this thread make only `n` more kqueues, until the returned guard drops.
    pub(crate) fn queues(n: usize) -> Allowance {
        Allowance::set(&QUEUES, n)
    }

    pub(super) fn make() -> std::io::Result<()> {
        spend(&QUEUES, "no kqueue may be made (test hook)")
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
pub(super) mod watch_tests;
