//! Interrupts held back while `run` has a launch held.
//!
//! From the `launchctl debug` that holds the next launch until the `SIGCONT` that resumes it, an
//! interrupt that ended goetia would leave the job's next launch held and stopped. So a [`Deferred`]
//! makes SIGINT, SIGTERM and SIGHUP be recorded instead, and re-raises each one the window recorded once
//! the last [`Deferred`] is dropped, to whatever handled it before. A signal that was ignored when the
//! window opened (`nohup` ignores SIGHUP) is not taken over: it stays ignored, its handler never runs, and
//! it is no interrupt.
//!
//! The handler is the one source of an interrupt. It records the signal and writes a byte to a pipe whose
//! read end a wait registers ([`Deferred::wake`]): a signal is an interrupt exactly when its handler has
//! run, and the wait wakes after the record, never before. Nothing reads the pipe, as the record is sticky
//! for the whole window, so a readable pipe means a recorded interrupt, on every queue it is registered
//! with.
//!
//! It is a process-wide handler, not a mask: `run` has already spawned the thread that waits for the run,
//! and a signal sent to the process goes to any thread that does not block it, so a mask on the calling
//! thread would protect nothing. The handlers are counted under one mutex: two `begin_run`s in one
//! process (library use) share the one set and the one pipe, and neither saves the other's as the previous.
//!
//! `signal-hook` was not used: its unregister leaves its own handler installed and chained, and it neither
//! restores the previous handler nor re-raises the signal.

use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use nix::fcntl::{FcntlArg, OFlag, fcntl};

use crate::error::{Error, Result};

/// The signals a terminal, a session or a service manager ends a process with, and their names. Bit `i` of
/// [`RECORDED`] is `SIGNALS[i]`.
const SIGNALS: [(libc::c_int, &str); 3] = [
    (libc::SIGINT, "SIGINT"),
    (libc::SIGTERM, "SIGTERM"),
    (libc::SIGHUP, "SIGHUP"),
];

/// What an open window holds.
struct Open {
    id: WindowId,
    /// The dispositions of [`SIGNALS`] before the first holder.
    previous: [libc::sigaction; 3],
    /// The mask of the signals the window took over (bit `i` for `SIGNALS[i]`): a signal that was ignored
    /// is left as it was.
    taken: u32,
    wake: Wake,
}

/// Who holds a [`Deferred`], and what to put back when the last one is dropped.
struct Shared {
    holders: usize,
    /// `Some` while there is a holder.
    open: Option<Open>,
    /// How many windows have been opened: the last one's [`WindowId`].
    opened: u64,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    holders: 0,
    open: None,
    opened: 0,
});

/// Which window a [`Deferred`] and what it hands out belong to: one per window, never reused in the
/// process. A descriptor number is not one, as the pipe of a later window may get the number of the pipe of
/// an earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowId(u64);

/// Bit `i` is set once `SIGNALS[i]` has arrived and nobody has claimed it yet.
static RECORDED: AtomicU32 = AtomicU32::new(0);

/// Every bit of [`RECORDED`].
const ALL: u32 = (1 << SIGNALS.len()) - 1;

/// Whether a window is open: set once its pipe is published, before its handlers are installed, cleared by
/// the drop of the last holder after the previous ones are back.
static OPEN: AtomicBool = AtomicBool::new(false);

/// The write end of the open window's pipe, which the handler writes to, or `-1`. It is published before the
/// first handler is installed and un-published before the pipe is closed.
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// How many handlers are in the part of theirs that reads [`WAKE`] and writes to it, and no longer: a
/// handler counts itself out before it claims its signal and raises it to the previous handler. So the
/// close, which un-publishes the descriptor first, knows when none can write to it, and waits for no more
/// than one `write` of each, never for the code of a previous handler, which may not return.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// The pipe a window's handler writes to and a wait reads, both ends non-blocking and close-on-exec.
struct Wake {
    read: OwnedFd,
    write: OwnedFd,
}

impl Wake {
    fn new() -> io::Result<Wake> {
        #[cfg(test)]
        test_hook::step()?;
        // Apple has no `pipe2`, so `std` sets close-on-exec on each end after the pipe is made, not with
        // it. A program started in between would inherit the ends, but nothing in goetia starts one while
        // a window opens: `run` starts its child from its own thread, in sequence after the window.
        let (read, write) = io::pipe()?;
        let (read, write): (OwnedFd, OwnedFd) = (read.into(), write.into());
        for end in [&read, &write] {
            let flags = OFlag::from_bits_truncate(fcntl(end, FcntlArg::F_GETFL).map_err(io::Error::from)?);
            fcntl(end, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
        }
        Ok(Wake { read, write })
    }
}

/// Takes the bits of `mask` out of [`RECORDED`], and answers those that were set: the caller that took a
/// bit is the one to re-raise it, so each recorded signal is re-raised exactly once.
fn claim(mask: u32) -> u32 {
    RECORDED.fetch_and(!mask, Ordering::SeqCst) & mask
}

/// The handler: records the signal and wakes the waits ([`wake`]), and where the window has closed already,
/// claims the signal back and raises it. It leaves `errno` as it found it.
///
/// A signal can be delivered on any thread, so a handler may still be running while the last holder is
/// dropped: its bit then lands after the drop has claimed what it saw. Both orders are covered: the drop
/// clears [`OPEN`] only after the previous handlers are back, so a handler that finds it clear raises to
/// them, and one that finds it set has recorded before the drop's claim, which then takes the bit.
///
/// Only async-signal-safe calls: atomics, `write`, `raise`.
extern "C" fn record(signal: libc::c_int) {
    let Some(bit) = SIGNALS.iter().position(|&(s, _)| s == signal) else {
        return;
    };
    let mask = 1 << bit;
    // SAFETY: `__error` is the calling thread's `errno`, which a handler may read and write.
    let errno = unsafe { *libc::__error() };
    wake(signal, mask);
    if !OPEN.load(Ordering::SeqCst) && claim(mask) != 0 {
        // SAFETY: `raise` takes a valid signal number.
        unsafe { libc::raise(signal) };
    }
    // SAFETY: as above.
    unsafe { *libc::__error() = errno };
}

/// Records `signal` (`mask` is its bit) and writes a byte to the open window's pipe, if one is published.
/// The only part of the handler that [`IN_FLIGHT`] counts: the previous handler it raises to afterwards is
/// no business of the close.
fn wake(signal: libc::c_int, mask: u32) {
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    // The record before the wake: a wait that the byte wakes finds the record set.
    RECORDED.fetch_or(mask, Ordering::SeqCst);
    let fd = WAKE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = signal as u8;
        // A pipe that is full already holds a byte that nothing has read, so a failed write loses nothing.
        // SAFETY: `fd` is open: the close un-publishes it, then waits for the handlers counted in, this one
        // included, before it closes it. `byte` is one valid byte.
        _ = unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
    IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
}

/// The interrupts a window had recorded at one read, which is what a decision and the error it ends with
/// both rest on: a signal that arrives after the read is in neither. Never empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Interrupts(u32);

/// "SIGINT", "SIGINT and SIGTERM", "SIGINT, SIGTERM and SIGHUP".
impl fmt::Display for Interrupts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = SIGNALS
            .iter()
            .enumerate()
            .filter(|&(bit, _)| self.0 & (1 << bit) != 0)
            .map(|(_, &(_, name))| name)
            .collect();
        match names.split_last() {
            None => Ok(()),
            Some((only, [])) => f.write_str(only),
            Some((last, rest)) => write!(f, "{} and {last}", rest.join(", ")),
        }
    }
}

/// One count of the window in which interrupts are recorded, not acted on. While any is alive the
/// handlers are goetia's; the last one dropped puts the previous ones back and re-raises what arrived.
pub(crate) struct Deferred {
    /// The mask of the signals the window took over, as when this holder joined it.
    taken: u32,
    /// The read end of the window's pipe, open for as long as a holder is.
    wake: RawFd,
    id: WindowId,
}

impl Deferred {
    /// Joins the window, opening it (making the pipe, saving the previous dispositions and installing the
    /// recording handler) if it is the first. An `Err` leaves every disposition as it was, and publishes
    /// nothing.
    ///
    /// # Exactly once, in this order
    ///
    /// Opening: the pipe is made and its write end published, then `OPEN` is set, **before** the first
    /// handler is installed, so that a handler that runs while the others are still being installed finds
    /// the window open and only records. (Set after, such a handler would find it closed, claim its own bit
    /// and `raise` into itself, and on this thread the redelivery would come before the store ever ran: a
    /// livelock.) Nothing of ours runs before the first install, so a signal before it goes to the previous
    /// handler as it always did.
    ///
    /// Both transitions, opening and closing, happen under the `SHARED` lock; so do the installs and
    /// restores of the handlers. The close does not release the lock until no handler is writing to the
    /// pipe (the wait on `IN_FLIGHT` after the descriptor is un-published), and the only code that touches
    /// `OPEN` or `RECORDED` outside the lock is the handler and the re-raise after a close, which acts on
    /// bits the close claimed. So a close cannot clear the `OPEN` of a window another `begin` opened. A
    /// handler the system had chosen but not yet started, or one past its write, finds `OPEN` of a later
    /// window or none: it records its signal, which that window re-raises, or raises it to the previous
    /// handler itself, and is neither lost nor raised twice.
    ///
    /// Closing, by the last drop or by an install that failed: the previous handlers are put back first,
    /// then the descriptor is un-published and the handlers in their write are waited for, then the pipe is
    /// closed, then `OPEN` is cleared, then every bit is claimed and re-raised. A handler `H` does
    /// `fetch_or` (A), reads `OPEN` (B), and if it is clear claims its bit (C) and raises it. All of these
    /// are `SeqCst`, so there is one order, and `claim` is a `fetch_and`, so a set bit is taken by exactly
    /// one of the claimers.
    ///
    /// - B reads set: B precedes the clear, so A precedes the closer's claim, which takes the bit.
    /// - B reads clear: the handlers are back already, so `H` (or the closer, if its claim came first)
    ///   raises the bit once, to the previous handler, never into ours.
    /// - A signal that arrives while the install is part way (some handlers ours, some previous) is
    ///   recorded if it is one of ours and goes to the previous handler if not; a failed install puts
    ///   back the ones it changed before it closes.
    pub(crate) fn begin() -> Result<Deferred> {
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.holders == 0 {
            let refused = |source| Error::os("hold back interrupts while a run is held", source);
            let wake = Wake::new().map_err(refused)?;
            WAKE.store(wake.write.as_raw_fd(), Ordering::SeqCst);
            OPEN.store(true, Ordering::SeqCst);
            match install() {
                Ok((previous, taken)) => {
                    shared.opened += 1;
                    let id = WindowId(shared.opened);
                    shared.open = Some(Open {
                        id,
                        previous,
                        taken,
                        wake,
                    });
                }
                Err(source) => {
                    // `install` put back what it changed. Closed under the lock, as `Drop` does, so a
                    // concurrent `begin` cannot open a window that this close then shuts; re-raised after
                    // it, since the handler reached may be anything.
                    let recorded = shut(wake);
                    drop(shared);
                    reraise(recorded);
                    return Err(refused(source));
                }
            }
        }
        shared.holders += 1;
        let open = shared.open.as_ref().expect("a window with a holder is open");
        Ok(Deferred {
            taken: open.taken,
            wake: open.wake.read.as_raw_fd(),
            id: open.id,
        })
    }

    /// Which window this holder is in.
    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    /// The read end of the window's pipe: readable once any interrupt is recorded, and for the rest of the
    /// window, as nothing reads it. A wait registers it and, when it wakes, reads [`Self::recorded`] again.
    /// It is valid for as long as this holder is alive.
    pub(crate) fn wake(&self) -> RawFd {
        self.wake
    }

    /// The interrupts recorded in the window so far, as of this one read, and `None` where there are none.
    /// Nothing is taken: the signals are still re-raised when the last window closes. Only the signals the
    /// window took over count.
    pub(crate) fn recorded(&self) -> Option<Interrupts> {
        let bits = RECORDED.load(Ordering::SeqCst) & self.taken;
        (bits != 0).then_some(Interrupts(bits))
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        let recorded = {
            let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
            shared.holders -= 1;
            if shared.holders > 0 {
                return;
            }
            let open = shared.open.take().expect("a window with a holder is open");
            restore(&open.previous, open.taken);
            shut(open.wake)
        };
        // Not under the lock: the handler it reaches may be anything.
        reraise(recorded);
    }
}

/// Closes the window, after the previous handlers are back: the pipe's write end is un-published, so that no
/// handler that starts from now on writes to it, the handlers in their write are waited for, under no
/// bound (a write to a non-blocking pipe ends on its own), and the pipe is closed. Then `OPEN` is cleared, so that a handler that finds
/// it clear raises to the previous handlers, and every recorded bit is claimed. The bits it answers are the
/// caller's to [`reraise`].
fn shut(wake: Wake) -> u32 {
    WAKE.store(-1, Ordering::SeqCst);
    while IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        std::thread::yield_now();
    }
    drop(wake);
    OPEN.store(false, Ordering::SeqCst);
    claim(ALL)
}

/// Sends each signal whose bit is in `recorded` again, to whatever handled it before the window.
///
/// The window took the signal over on every thread that did not block it, so the calling thread may be one
/// that blocks it: `raise`, which sends to the calling thread only, would leave it pending there for as long
/// as the block lasts. Such a signal is sent to the process, which gives it to a thread that takes it. One
/// the calling thread takes is raised on it, so that the previous handler has run when this returns, as a
/// signal sent to the process need not have (any thread may be the one to take it).
fn reraise(recorded: u32) {
    for (bit, &(signal, _)) in SIGNALS.iter().enumerate() {
        if recorded & (1 << bit) != 0 {
            // SAFETY: `raise` takes a valid signal number; `kill` takes plain integers.
            let sent = unsafe {
                if blocked_here(signal) {
                    libc::kill(libc::getpid(), signal)
                } else {
                    libc::raise(signal)
                }
            };
            debug_assert_eq!(sent, 0, "re-raising {signal} failed");
        }
    }
}

/// Whether the calling thread blocks `signal`.
fn blocked_here(signal: libc::c_int) -> bool {
    // SAFETY: an all-zero `sigset_t` is a valid place for one; a null new set only reads the mask into it.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        let read = libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask);
        debug_assert_eq!(read, 0, "read the signal mask");
        libc::sigismember(&mask, signal) == 1
    }
}

/// A `sigaction` for `handler` with no flags and an empty mask.
fn action(handler: libc::sighandler_t) -> libc::sigaction {
    // SAFETY: an all-zero `sigaction` is a valid value (no handler, no flags); the mask is then emptied.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    // SAFETY: `action.sa_mask` is a valid `sigset_t`.
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    action
}

/// Installs [`record`] for the signals of [`SIGNALS`] that are not ignored, giving back what each had and
/// the mask of those it took over. A signal that is ignored (`nohup` ignores SIGHUP) is left alone: the
/// user set it so, and it stays so. One that the calling thread blocks is taken over all the same: the
/// handler is the process's, and another thread that does not block it can be the one it is delivered to.
/// All or none.
fn install() -> io::Result<([libc::sigaction; 3], u32)> {
    let mut ours = action(record as extern "C" fn(libc::c_int) as libc::sighandler_t);
    // A call that a handler interrupts, on any thread, is restarted: the pipe ends the wait that has to end
    // on an interrupt, and the others (a `launchctl` run, a read of a file) do not see it.
    ours.sa_flags = libc::SA_RESTART;

    let mut previous = [action(libc::SIG_DFL); 3];
    let mut taken = 0;
    for (i, &(signal, _)) in SIGNALS.iter().enumerate() {
        // SAFETY: a null new action only reads, into `previous[i]`, a valid place for the old one.
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous[i]) } != 0 {
            let failed = io::Error::last_os_error();
            restore(&previous[..i], taken);
            return Err(failed);
        }
        if previous[i].sa_sigaction == libc::SIG_IGN {
            continue;
        }
        #[cfg(test)]
        if let Err(refused) = test_hook::step() {
            restore(&previous[..i], taken);
            return Err(refused);
        }
        // SAFETY: `ours` is a valid `sigaction`, and `previous[i]` a valid place for the old one.
        if unsafe { libc::sigaction(signal, &ours, &mut previous[i]) } != 0 {
            let failed = io::Error::last_os_error();
            restore(&previous[..i], taken);
            return Err(failed);
        }
        taken |= 1 << i;
    }
    Ok((previous, taken))
}

/// Puts `previous` back for the first signals of [`SIGNALS`] that `taken` has a bit for. Cleanup: it never
/// fails the caller.
fn restore(previous: &[libc::sigaction], taken: u32) {
    for (bit, (&(signal, _), old)) in SIGNALS.iter().zip(previous).enumerate() {
        if taken & (1 << bit) == 0 {
            continue;
        }
        // SAFETY: `old` is a `sigaction` this process read from the kernel.
        let put_back = unsafe { libc::sigaction(signal, old, std::ptr::null_mut()) };
        debug_assert_eq!(put_back, 0, "restoring the handler of signal {signal} failed");
    }
}

#[cfg(test)]
mod test_hook {
    use std::cell::Cell;

    use crate::backend::test_allowance::{Allowance, spend};

    thread_local! {
        static STEPS: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Let this thread take only `n` more of a window's steps, until the returned guard drops: the pipe
    /// first, then each handler.
    pub(super) fn steps(n: usize) -> Allowance {
        Allowance::set(&STEPS, n)
    }

    pub(super) fn step() -> std::io::Result<()> {
        spend(&STEPS, "no step of opening a window may be taken (test hook)")
    }
}

#[cfg(test)]
#[path = "interrupts_tests.rs"]
pub(super) mod interrupts_tests;
