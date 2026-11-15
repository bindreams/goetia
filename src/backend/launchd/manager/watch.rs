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
//! process's. One [`Watch`] serves every pid of one verb, and a registration reads only its own pid's
//! events: an event it reads for another registration's pid is kept for that one, never dropped.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

use crate::error::{Error, Result};
use crate::manager::budget::Deadline;

/// One kqueue fd.
pub(crate) struct Watch {
    shared: Arc<Shared>,
}

/// The queue, and the exits read from it for a registration other than the one that read them.
struct Shared {
    queue: Kqueue,
    /// Taken out of the queue by one registration's `wait` or drop and not its own, so kept here for the
    /// registration whose pid it is. `status` is `None` for an exit the kernel reported without a status.
    kept: Mutex<Vec<Exited>>,
}

/// One pid's exit, read from the queue.
struct Exited {
    pid: u32,
    status: Option<i32>,
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
    shared: Arc<Shared>,
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
            .map(|queue| Watch {
                shared: Arc::new(Shared {
                    queue,
                    kept: Mutex::new(Vec::new()),
                }),
            })
            .map_err(|source| Error::os("make a kqueue to watch processes exit with", source))
    }

    /// Registers `pid` for its exit and the status it exits with. [`Added::Gone`] where there is no such
    /// process. Registrations are waited on one at a time, by one thread.
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
            match self.shared.queue.kevent(&[change], &mut receipt, Some(now)) {
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
                shared: Arc::clone(&self.shared),
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
            if let Some(exited) = self.take_kept() {
                return exited.status.map(Some).ok_or_else(|| {
                    Error::os(
                        format!("wait for process {pid} to exit"),
                        io::Error::other("the kernel reported its exit without its status"),
                    )
                });
            }
            // Asked again with what is left each time: the kernel's clock is not the deadline's, and
            // a call that returns early finds the deadline not yet spent.
            let remaining = deadline.and_then(|deadline| deadline.remaining());
            let timeout = remaining.map(timespec);
            match self.shared.queue.kevent(&[], &mut events, timeout) {
                Ok(0) => {
                    if deadline.is_some_and(|deadline| deadline.expired()) {
                        return Ok(None);
                    }
                }
                // Every exit read is kept, this pid's included: the next turn takes it.
                Ok(n) => self.shared.keep(&events[..n], None),
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
            match self.shared.queue.kevent(&[change], &mut receipt, Some(now)) {
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

    /// Takes this pid's exit out of what other reads kept, if one did.
    fn take_kept(&self) -> Option<Exited> {
        let mut kept = self.shared.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let at = kept.iter().position(|exited| exited.pid == self.pid)?;
        Some(kept.swap_remove(at))
    }

    /// Removes every event of this pid from the queue and from what was kept, and leaves any other pid's
    /// kept for its registration. Whatever `delete` did or did not remove, no event of this pid is left.
    ///
    /// Reads with a zero timeout until a read finds the queue empty; it ends because each live
    /// registration adds at most one exit and this adds none.
    fn drain(&self) {
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut events = [blank(); 8];
        loop {
            match self.shared.queue.kevent(&[], &mut events, Some(now)) {
                Ok(0) => break,
                Ok(n) => self.shared.keep(&events[..n], Some(self.pid)),
                Err(Errno::EINTR) => {}
                // A queue that cannot be read holds nothing a later wait could be handed.
                Err(_) => break,
            }
        }
        self.shared
            .kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|exited| exited.pid != self.pid);
    }
}

impl Shared {
    /// Keeps the exits among `events`, except `pid`'s where it is given: those are the registration's own
    /// and are being discarded.
    fn keep(&self, events: &[KEvent], pid: Option<u32>) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.extend(
            events
                .iter()
                .filter_map(exited)
                .filter(|exited| Some(exited.pid) != pid),
        );
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

/// The exit `event` reports: `None` for any other event.
fn exited(event: &KEvent) -> Option<Exited> {
    let is_exit = event.filter() == Ok(EventFilter::EVFILT_PROC) && event.fflags().contains(FilterFlag::NOTE_EXIT);
    is_exit.then(|| Exited {
        pid: event.ident() as u32,
        status: event
            .fflags()
            .contains(FilterFlag::NOTE_EXITSTATUS)
            .then(|| event.data() as i32),
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
