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
//! [interruptible](Watch::new_interruptible) watch joins the window the caller has open, with a
//! [`Deferred`] of its own, and has the read end of that window's pipe registered on its queue; the handler
//! writes to the pipe after it records a signal. So a wait that blocks wakes on an interrupt, and a wake
//! means only "read the record again": the wait reads the window's record before it blocks and after every
//! wake, and the record is the interrupt. Each launch's registration holds one too, so that its waits read
//! the record of the window their watch's pipe belongs to and no other's; [`Registered::leave_window`] lets
//! it go, as the [`Run`](super::run::Run) that outlives the window's end must. The registration is
//! `EV_CLEAR`: an event that a read of the queue takes (a poll for an exit included) is not there again, so
//! a wait with no window (a `Run`'s, while another holder keeps the pipe open) is not woken over and over
//! by it, and nothing is lost, as every wait with a window reads the record next.

use std::io;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

use super::interrupts::{Deferred, Interrupts};
use crate::error::{Error, Result};
use crate::manager::budget::Deadline;

/// One kqueue fd.
pub(crate) struct Watch {
    shared: Arc<Shared>,
    /// The window whose pipe is registered on the queue, held for as long as the watch is: a window that
    /// closes and a later one that opens cannot take its place, whichever descriptor numbers the pipes get.
    /// `None` for a watch that cannot be woken by an interrupt.
    window: Option<Deferred>,
}

/// The queue, and the events read from it for a registration other than the one that read them.
struct Shared {
    queue: Kqueue,
    /// Taken out of the queue by one registration's `wait` or drop and not its own, so kept here for the
    /// registration whose pid it is. `status` is `None` for an exit the kernel reported without a status.
    kept: Mutex<Vec<Exited>>,
    /// The pids whose exec was read the same way, once each: later execs of a pid add nothing.
    execed: Mutex<Vec<u32>>,
}

/// One pid's exit, read from the queue.
#[derive(Clone, Copy)]
pub(crate) struct Exited {
    pid: u32,
    /// The raw `wait(2)` status, `None` for an exit the kernel reported without one.
    pub(crate) status: Option<i32>,
}

impl Exited {
    /// The raw `wait(2)` status, for a caller that cannot go on without it: an exit the kernel reported
    /// with none is an error.
    pub(crate) fn raw_status(self) -> Result<i32> {
        self.status.ok_or_else(|| {
            Error::os(
                format!("wait for process {} to exit", self.pid),
                io::Error::other("the kernel reported its exit without its status"),
            )
        })
    }
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
    /// The watch's window, joined for a launch ([`Watch::add_launch`]) of an interruptible watch: what its
    /// waits for an exec and an exit read. `None` for any other registration.
    window: Option<Deferred>,
}

/// What ended a wait of [`Registered::first_exec`] or [`Registered::exit`].
pub(crate) enum Woke {
    /// The pid's first exec. Only [`Registered::first_exec`] answers it.
    Exec,
    /// The pid exited, with its `wait(2)` status where the kernel reported one.
    Exited(Option<i32>),
    /// An interrupt: SIGINT, SIGTERM or SIGHUP arrived in the [`Deferred`] window. The interrupts are the
    /// record the wait ended on.
    Interrupted(Interrupts),
}

/// The longest a single `kevent` call is asked to block, in seconds. A `wait` under a longer deadline
/// asks again with what is left, so this is a limit of the call and decides nothing.
const LONGEST_CALL_SECS: u64 = i32::MAX as u64;

impl Watch {
    /// Made before a verb sends its first request, so that failing to make it (a process out of file
    /// descriptors) strands nothing.
    pub(crate) fn new() -> Result<Watch> {
        Watch::make(None)
    }

    /// A watch that joins `window`, and whose queue also has the read end of its pipe registered
    /// ([`Deferred::wake`]), for the waits that end on an interrupt. The window is the caller's to open,
    /// before this, so that the signals are taken over before the queue is made, and a watch cannot open
    /// a window of its own. The pipe is closed when the window's last holder is dropped, which removes the
    /// registration with it.
    pub(crate) fn new_interruptible(window: &Deferred) -> Result<Watch> {
        Watch::make(Some(window.join()))
    }

    /// The interrupts the watch's window has recorded so far, as of this one read: `None` where there are
    /// none, and for a watch that is not interruptible.
    pub(crate) fn recorded(&self) -> Option<Interrupts> {
        recorded_in(self.window.as_ref())
    }

    fn make(window: Option<Deferred>) -> Result<Watch> {
        let queue = make_queue().map_err(|source| Error::os("make a kqueue to watch processes exit with", source))?;
        let watch = Watch {
            shared: Arc::new(Shared {
                queue,
                kept: Mutex::new(Vec::new()),
                execed: Mutex::new(Vec::new()),
            }),
            window,
        };
        if let Some(window) = &watch.window {
            watch
                .register_wake(window.wake())
                .map_err(|source| Error::os("watch for interrupts on the kqueue", source))?;
        }
        Ok(watch)
    }

    /// Registers the read end `wake` of a window's pipe, answered by its own receipt. `EV_CLEAR`: it is an
    /// event for each write to the pipe, and one that a read of the queue takes is not there again.
    fn register_wake(&self, wake: RawFd) -> io::Result<()> {
        let change = KEvent::new(
            wake as usize,
            EventFilter::EVFILT_READ,
            EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_CLEAR | EvFlags::EV_RECEIPT,
            FilterFlag::empty(),
            0,
            0,
        );
        let mut receipt = [blank()];
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let answered = loop {
            match self.shared.queue.kevent(&[change], &mut receipt, Some(now)) {
                Err(Errno::EINTR) => {}
                other => break other.map_err(io::Error::from)?,
            }
        };
        debug_assert_eq!(answered, 1, "EV_RECEIPT answers each change");
        match receipt[0].data() {
            0 => Ok(()),
            code => Err(io::Error::from(Errno::from_raw(code as i32))),
        }
    }

    /// Registers `pid` for its exit and the status it exits with. [`Added::Gone`] where there is no such
    /// process. Registrations are waited on one at a time, by one thread.
    ///
    /// A registration that succeeds is a registration whose exit is reported: none missed it in 535,000
    /// passes (a root prober, a target that is not its child). The kernel refuses (`ESRCH`, [`Added::Gone`])
    /// from the start of the process's exit, before `NOTE_EXIT` is posted, and refuses a zombie its parent
    /// has not reaped. Measured on macOS CI (KB cc08f57: "macOS - A successful NOTE_EXIT registration on a
    /// process that is not the caller's child never misses the exit", "macOS - kqueue refuses a NOTE_EXIT
    /// registration once a process's exit has begun, before NOTE_EXIT is posted and before the zombie",
    /// "macOS - kqueue refuses a NOTE_EXIT registration on a zombie its parent has not reaped").
    pub(crate) fn add(&self, pid: u32) -> Result<Added> {
        self.register(
            pid,
            EvFlags::empty(),
            FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
            None,
        )
    }

    /// [`Self::add`] for a launch `run` follows: `pid` is also registered for its exec, which the waits
    /// report, and the registration joins the watch's window, if it has one. With `EV_CLEAR`, which an
    /// exec that repeats needs to be read once per time: the flags are the ones measured on macOS CI (KB,
    /// "macOS - NOTE_EXEC reaches a kqueue on a held launch's pid, and the process reads stopped from then
    /// until SIGCONT").
    pub(crate) fn add_launch(&self, pid: u32) -> Result<Added> {
        let window = self.window.as_ref().map(Deferred::join);
        self.register(
            pid,
            EvFlags::EV_CLEAR,
            FilterFlag::NOTE_EXEC | FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
            window,
        )
    }

    fn register(&self, pid: u32, flags: EvFlags, fflags: FilterFlag, window: Option<Deferred>) -> Result<Added> {
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
                window,
            })),
            code => match Errno::from_raw(code as i32) {
                Errno::ESRCH => Ok(Added::Gone),
                errno => Err(watching(errno.into())),
            },
        }
    }
}

impl Registered {
    /// Blocks until the pid has exited, and answers the exit: its status is `None` where the kernel
    /// reported the exit without one ([`Exited::raw_status`] makes that an error, for a caller that needs
    /// it). `None` iff `deadline` expired first, and none expires without one. An exit that happened before
    /// the wait began is reported at once.
    pub(crate) fn wait(self, deadline: Option<Deadline>) -> Result<Option<Exited>> {
        let pid = self.pid;
        match self.next(false, None, deadline)? {
            Some(Woke::Exited(status)) => Ok(Some(Exited { pid, status })),
            None => Ok(None),
            Some(Woke::Exec | Woke::Interrupted(_)) => unreachable!("neither was waited for"),
        }
    }

    /// Lets go of the registration's hold on the window, which the registration of a launch that has been
    /// resumed must not keep: a [`Run`](super::run::Run) lasts as long as the process does, and interrupts
    /// are held back only until the `SIGCONT`.
    pub(crate) fn leave_window(mut self) -> Registered {
        self.window = None;
        self
    }

    /// The exit of the pid, if it has happened: read without blocking and without taking the exit. The queue
    /// is read until it is empty, what it holds is kept for the waits, and the exit, if one is kept, is
    /// reported with its status. `run` accepts a read of the pid by number only where this says none after
    /// it: an exit is no later than its event, and until the pid is reaped it names no other process.
    pub(crate) fn exit_kept(&self) -> Result<Option<Exited>> {
        let pid = self.pid;
        self.poll(None)
            .map_err(|errno| Error::os(format!("poll process {pid} for its exit"), io::Error::from(errno)))?;
        let kept = self.shared.kept.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(kept.iter().find(|exited| exited.pid == pid).copied())
    }

    /// Reads the queue with a zero timeout until a read finds it empty, and keeps what it held
    /// ([`Shared::keep`]) except the events of `discarding`, a pid whose registration is being dropped. It
    /// ends because each live registration adds at most one exit, and an exec or a wake is an event only as
    /// it happens.
    fn poll(&self, discarding: Option<u32>) -> std::result::Result<(), Errno> {
        let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut events = [blank(); 8];
        loop {
            match self.shared.queue.kevent(&[], &mut events, Some(now)) {
                Ok(0) => return Ok(()),
                Ok(n) => self.shared.keep(&events[..n], discarding),
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno),
            }
        }
    }

    /// Blocks until the launch's pid has exec'd for the first time, exited, or its window has recorded an
    /// interrupt: [`Woke::Exec`] (also where its exit came with it, which a [`Self::exit`] then reports),
    /// [`Woke::Exited`] or [`Woke::Interrupted`]. Under no bound. A registration that has no window
    /// (anything but [`Watch::add_launch`] on an [interruptible](Watch::new_interruptible) watch, or one
    /// that has [left](Self::leave_window) it) is never interrupted.
    pub(crate) fn first_exec(&self) -> Result<Woke> {
        Ok(self
            .next(true, self.window.as_ref(), None)?
            .expect("a wait under no deadline never expires"))
    }

    /// Blocks until the pid has exited ([`Woke::Exited`], whose status is of no concern to a caller that
    /// does not read it) or its window has recorded an interrupt. Under no bound. As for
    /// [`Self::first_exec`], a registration with no window is never interrupted.
    pub(crate) fn exit(self) -> Result<Woke> {
        Ok(self
            .next(false, self.window.as_ref(), None)?
            .expect("a wait under no deadline never expires"))
    }

    /// The first of: this pid's exec (where `exec`), its exit, an interrupt (where there is a `window`
    /// and the watch is interruptible), and the `deadline` (`None` iff it expired first).
    ///
    /// The window's record is read first, so that an exec or an exit that was kept with an interrupt never
    /// answers the wait: the caller resumes or follows nothing after an interrupt. Then what was kept is
    /// taken, then the queue is waited on: an interrupt before this point is in the record, and one after it
    /// writes the pipe, which the queue has registered already. Any wake, and an `EINTR`, only sends the wait
    /// back to the record, which the handler sets before it writes.
    fn next(&self, exec: bool, window: Option<&Deferred>, deadline: Option<Deadline>) -> Result<Option<Woke>> {
        let pid = self.pid;
        let waited_for = if exec { "exec" } else { "exit" };
        let waiting = |errno: Errno| {
            Error::os(
                format!("wait for process {pid} to {waited_for}"),
                io::Error::from(errno),
            )
        };
        let mut events = [blank(); 8];
        loop {
            if let Some(interrupts) = recorded_in(window) {
                return Ok(Some(Woke::Interrupted(interrupts)));
            }
            if exec && self.take_exec() {
                return Ok(Some(Woke::Exec));
            }
            if let Some(exited) = self.take_kept() {
                return Ok(Some(Woke::Exited(exited.status)));
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
                // Every exit and exec read is kept, this pid's included, and a wake is none of them: the next
                // turn takes them and reads the record.
                Ok(n) => self.shared.keep(&events[..n], None),
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
    fn drain(&self) {
        // A queue that cannot be read holds nothing a later wait could be handed.
        _ = self.poll(Some(self.pid));
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

/// The interrupts `window` has recorded, as of this one read, and `None` where there are none or no window:
/// the one read of the record, which a [`Watch`] and a wait share.
fn recorded_in(window: Option<&Deferred>) -> Option<Interrupts> {
    window.and_then(Deferred::recorded)
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
