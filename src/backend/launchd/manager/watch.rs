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
//!
//! `run` watches a launch ([`Watch::add_launch`]): its pid is also registered for `NOTE_EXEC`, and its
//! waits ([`Registered::first_exec`], [`Registered::exit`]) end when an interrupt does. An
//! [interruptible](Watch::new_interruptible) watch has `SIGINT`, `SIGTERM` and `SIGHUP` registered on its
//! queue, so a signal that arrives while a wait blocks wakes it. A signal that arrived before the wait
//! has no event, so the wait reads the [`Deferred`] window's record after the registration and before it
//! blocks, and again before each block.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

use super::interrupts::{Deferred, SIGNALS};
use crate::error::{Error, Result};
use crate::manager::budget::Deadline;

/// One kqueue fd.
pub(crate) struct Watch {
    shared: Arc<Shared>,
}

/// The queue, and the events read from it for a registration other than the one that read them.
struct Shared {
    queue: Kqueue,
    /// Whether the queue has [`SIGNALS`] registered: only then can a wait be given a [`Deferred`] window.
    interruptible: bool,
    /// Taken out of the queue by one registration's `wait` or drop and not its own, so kept here for the
    /// registration whose pid it is. `status` is `None` for an exit the kernel reported without a status.
    kept: Mutex<Vec<Exited>>,
    /// The pids whose exec was read the same way, once each: later execs of a pid add nothing.
    execed: Mutex<Vec<u32>>,
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

/// What ended a wait of [`Registered::first_exec`] or [`Registered::exit`].
pub(crate) enum Woke {
    /// The pid's first exec. Only [`Registered::first_exec`] answers it.
    Exec,
    /// The pid exited, with its `wait(2)` status where the kernel reported one.
    Exited(Option<i32>),
    /// An interrupt: SIGINT, SIGTERM or SIGHUP arrived in the [`Deferred`] window.
    Interrupted,
}

/// The longest a single `kevent` call is asked to block, in seconds. A `wait` under a longer deadline
/// asks again with what is left, so this is a limit of the call and decides nothing.
const LONGEST_CALL_SECS: u64 = i32::MAX as u64;

impl Watch {
    /// Made before a verb sends its first request, so that failing to make it (a process out of file
    /// descriptors) strands nothing.
    pub(crate) fn new() -> Result<Watch> {
        Watch::make(false)
    }

    /// A watch whose queue also has [`SIGNALS`] registered, for the waits that end on an interrupt. Its
    /// signals are the process's own, so a test that raises one takes the whole process's turn.
    pub(crate) fn new_interruptible() -> Result<Watch> {
        Watch::make(true)
    }

    fn make(interruptible: bool) -> Result<Watch> {
        let queue = make_queue().map_err(|source| Error::os("make a kqueue to watch processes exit with", source))?;
        let watch = Watch {
            shared: Arc::new(Shared {
                queue,
                interruptible,
                kept: Mutex::new(Vec::new()),
                execed: Mutex::new(Vec::new()),
            }),
        };
        if interruptible {
            watch
                .register_signals()
                .map_err(|source| Error::os("watch for interrupts on the kqueue", source))?;
        }
        Ok(watch)
    }

    /// Registers each of [`SIGNALS`], answered by its own receipt.
    fn register_signals(&self) -> io::Result<()> {
        let changes = SIGNALS.map(|signal| {
            KEvent::new(
                signal as usize,
                EventFilter::EVFILT_SIGNAL,
                EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_RECEIPT,
                FilterFlag::empty(),
                0,
                0,
            )
        });
        let mut receipts = [blank(); SIGNALS.len()];
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let answered = loop {
            match self.shared.queue.kevent(&changes, &mut receipts, Some(now)) {
                Err(Errno::EINTR) => {}
                other => break other.map_err(io::Error::from)?,
            }
        };
        debug_assert_eq!(answered, SIGNALS.len(), "EV_RECEIPT answers each change");
        for receipt in &receipts[..answered] {
            match receipt.data() {
                0 => {}
                code => return Err(io::Error::from(Errno::from_raw(code as i32))),
            }
        }
        Ok(())
    }

    /// Registers `pid` for its exit and the status it exits with. [`Added::Gone`] where there is no such
    /// process. Registrations are waited on one at a time, by one thread.
    pub(crate) fn add(&self, pid: u32) -> Result<Added> {
        self.register(
            pid,
            EvFlags::empty(),
            FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
        )
    }

    /// [`Self::add`] for a launch `run` follows: `pid` is also registered for its exec, which the waits
    /// that take an interrupt window report. With `EV_CLEAR`, which an exec that repeats needs to be read
    /// once per time: the flags are the ones measured on macOS CI (KB, "macOS - NOTE_EXEC reaches a
    /// kqueue on a held launch's pid, and the process reads stopped from then until SIGCONT").
    pub(crate) fn add_launch(&self, pid: u32) -> Result<Added> {
        self.register(
            pid,
            EvFlags::EV_CLEAR,
            FilterFlag::NOTE_EXEC | FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
        )
    }

    fn register(&self, pid: u32, flags: EvFlags, fflags: FilterFlag) -> Result<Added> {
        let change = KEvent::new(
            pid as usize,
            EventFilter::EVFILT_PROC,
            // A receipt, so that the answer for this one change comes back in `receipt`, whatever the
            // queue already holds.
            EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_RECEIPT | flags,
            fflags,
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
        match self.next(false, None, deadline)? {
            Some(Woke::Exited(Some(status))) => Ok(Some(status)),
            Some(Woke::Exited(None)) => Err(Error::os(
                format!("wait for process {pid} to exit"),
                io::Error::other("the kernel reported its exit without its status"),
            )),
            None => Ok(None),
            Some(Woke::Exec | Woke::Interrupted) => unreachable!("neither was waited for"),
        }
    }

    /// Blocks until the launch's pid has exec'd for the first time, exited, or `window` has recorded an
    /// interrupt: [`Woke::Exec`] (also where its exit came with it, which a [`Self::exit`] then reports),
    /// [`Woke::Exited`] or [`Woke::Interrupted`]. Under no bound. Only for a registration of
    /// [`Watch::add_launch`] on an [interruptible](Watch::new_interruptible) watch.
    pub(crate) fn first_exec(&self, window: &Deferred) -> Result<Woke> {
        Ok(self
            .next(true, Some(window), None)?
            .expect("a wait under no deadline never expires"))
    }

    /// Blocks until the pid has exited ([`Woke::Exited`], whose status is of no concern to a caller that
    /// does not read it) or `window` has recorded an interrupt. Under no bound. Only on an
    /// [interruptible](Watch::new_interruptible) watch.
    pub(crate) fn exit(self, window: &Deferred) -> Result<Woke> {
        Ok(self
            .next(false, Some(window), None)?
            .expect("a wait under no deadline never expires"))
    }

    /// The first of: this pid's exec (where `exec`), its exit, an interrupt (where there is a `window`
    /// and the watch is interruptible), and the `deadline` (`None` iff it expired first).
    ///
    /// What was kept is taken first, then the window's record is read, then the queue is waited on: an
    /// interrupt before this point is in the record, and one after it is an event of the queue, which is
    /// registered already. An event of the queue wakes the wait whether or not the handler has recorded the
    /// signal yet, as the signal's generation and its handler's run are not ordered.
    fn next(&self, exec: bool, window: Option<&Deferred>, deadline: Option<Deadline>) -> Result<Option<Woke>> {
        debug_assert!(
            window.is_none() || self.shared.interruptible,
            "a window needs signals registered on the queue"
        );
        let pid = self.pid;
        let waiting = |errno: Errno| Error::os(format!("wait for process {pid} to exit"), io::Error::from(errno));
        let mut events = [blank(); 8];
        loop {
            if exec && self.take_exec() {
                return Ok(Some(Woke::Exec));
            }
            if let Some(exited) = self.take_kept() {
                return Ok(Some(Woke::Exited(exited.status)));
            }
            if window.is_some_and(Deferred::interrupted) {
                return Ok(Some(Woke::Interrupted));
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
                // Every exit and exec read is kept, this pid's included: the next turn takes it.
                Ok(n) => {
                    self.shared.keep(&events[..n], None);
                    if window.is_some() && events[..n].iter().any(signalled) {
                        return Ok(Some(Woke::Interrupted));
                    }
                }
                // A signal handled on this thread ends the call: the window's record is read again.
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(waiting(errno)),
            }
        }
    }

    /// Takes this pid's first exec out of what other reads kept, if one did.
    fn take_exec(&self) -> bool {
        let mut execed = self.shared.execed.lock().unwrap_or_else(PoisonError::into_inner);
        match execed.iter().position(|&pid| pid == self.pid) {
            Some(at) => {
                execed.swap_remove(at);
                true
            }
            None => false,
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
    /// registration adds at most one exit, and an exec or a signal is an event only as it happens.
    /// A signal's event is dropped here: whoever waits next reads the window's record first.
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
        self.shared
            .execed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|&pid| pid != self.pid);
    }
}

impl Shared {
    /// Keeps the exits and the first execs among `events`, except `pid`'s where it is given: those are
    /// the registration's own and are being discarded.
    fn keep(&self, events: &[KEvent], pid: Option<u32>) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.extend(
            events
                .iter()
                .filter_map(exited)
                .filter(|exited| Some(exited.pid) != pid),
        );
        drop(kept);
        let mut execed = self.execed.lock().unwrap_or_else(PoisonError::into_inner);
        for execing in events
            .iter()
            .filter_map(exec_of)
            .filter(|&execing| Some(execing) != pid)
        {
            if !execed.contains(&execing) {
                execed.push(execing);
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

/// The pid whose exec `event` reports: `None` for any other event.
fn exec_of(event: &KEvent) -> Option<u32> {
    let is_exec = event.filter() == Ok(EventFilter::EVFILT_PROC) && event.fflags().contains(FilterFlag::NOTE_EXEC);
    is_exec.then(|| event.ident() as u32)
}

/// Whether `event` is a signal's.
fn signalled(event: &KEvent) -> bool {
    event.filter() == Ok(EventFilter::EVFILT_SIGNAL)
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
