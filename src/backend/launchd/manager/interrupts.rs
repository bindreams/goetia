//! Interrupts held back while `run` has a launch held.
//!
//! From the `launchctl debug` that holds the next launch until the `SIGCONT` that resumes it, an
//! interrupt that ended goetia would leave the job's next launch held and stopped. So a [`Deferred`]
//! makes SIGINT, SIGTERM and SIGHUP be recorded instead, and re-raises each one the window recorded once
//! the last [`Deferred`] is dropped, to whatever handled it before.
//!
//! It is a process-wide handler, not a mask: `run` has already spawned the thread that waits for the run,
//! and a signal sent to the process goes to any thread that does not block it, so a mask on the calling
//! thread would protect nothing. The handlers are counted under one mutex: two `begin_run`s in one
//! process (library use) share the one set, and neither saves the other's as the previous.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::error::{Error, Result};

/// The signals a terminal, a session or a service manager ends a process with.
const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Who holds a [`Deferred`], and what to put back when the last one is dropped.
struct Shared {
    holders: usize,
    /// The dispositions of [`SIGNALS`] before the first holder, while there is one.
    previous: Option<[libc::sigaction; 3]>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    holders: 0,
    previous: None,
});

/// Bit `i` is set once `SIGNALS[i]` has arrived and nobody has claimed it yet.
static RECORDED: AtomicU32 = AtomicU32::new(0);

/// Every bit of [`RECORDED`].
const ALL: u32 = (1 << SIGNALS.len()) - 1;

/// Whether a window is open: set once its handlers are installed, cleared by the drop of the last holder
/// after the previous ones are back.
static OPEN: AtomicBool = AtomicBool::new(false);

/// Takes the bits of `mask` out of [`RECORDED`], and answers those that were set: the caller that took a
/// bit is the one to re-raise it, so each recorded signal is re-raised exactly once.
fn claim(mask: u32) -> u32 {
    RECORDED.fetch_and(!mask, Ordering::SeqCst) & mask
}

/// The handler: records the signal, and where the window has closed already, claims it back and raises
/// it. A signal can be delivered on any thread, so a handler may still be running while the last holder is
/// dropped: its bit then lands after the drop has claimed what it saw. Both orders are covered: the drop
/// clears [`OPEN`] only after the previous handlers are back, so a handler that finds it clear raises to
/// them, and one that finds it set has recorded before the drop's claim, which then takes the bit.
///
/// Only async-signal-safe calls: atomics, `raise`.
extern "C" fn record(signal: libc::c_int) {
    let Some(bit) = SIGNALS.iter().position(|&s| s == signal) else {
        return;
    };
    let mask = 1 << bit;
    RECORDED.fetch_or(mask, Ordering::SeqCst);
    if !OPEN.load(Ordering::SeqCst) && claim(mask) != 0 {
        // SAFETY: `raise` takes a valid signal number.
        unsafe { libc::raise(signal) };
    }
}

/// One count of the window in which interrupts are recorded, not acted on. While any is alive the
/// handlers are goetia's; the last one dropped puts the previous ones back and re-raises what arrived.
pub(crate) struct Deferred(());

impl Deferred {
    /// Joins the window, opening it (saving the previous dispositions and installing the recording
    /// handler) if it is the first. An `Err` leaves every disposition as it was.
    pub(crate) fn begin() -> Result<Deferred> {
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.holders == 0 {
            let previous = install().map_err(|source| Error::os("hold back interrupts while a run is held", source))?;
            shared.previous = Some(previous);
            OPEN.store(true, Ordering::SeqCst);
        }
        shared.holders += 1;
        Ok(Deferred(()))
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
            if let Some(previous) = shared.previous.take() {
                restore(&previous);
            }
            // After the previous handlers are back: a handler that finds the window closed raises to them.
            OPEN.store(false, Ordering::SeqCst);
            claim(ALL)
        };
        // Not under the lock: the handler it reaches may be anything.
        for (bit, &signal) in SIGNALS.iter().enumerate() {
            if recorded & (1 << bit) != 0 {
                // SAFETY: `raise` takes a valid signal number.
                let raised = unsafe { libc::raise(signal) };
                debug_assert_eq!(raised, 0, "raise({signal}) failed");
            }
        }
    }
}

/// Installs [`record`] for [`SIGNALS`], giving back what each had. All or none.
fn install() -> io::Result<[libc::sigaction; 3]> {
    // SAFETY: an all-zero `sigaction` is a valid value (no handler, no flags); the mask is then emptied.
    let mut ours: libc::sigaction = unsafe { std::mem::zeroed() };
    ours.sa_sigaction = record as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // Calls interrupted by a signal are restarted, so a wait in this window does not see the interrupt.
    ours.sa_flags = libc::SA_RESTART;
    // SAFETY: `ours.sa_mask` is a valid `sigset_t`.
    unsafe { libc::sigemptyset(&mut ours.sa_mask) };

    // SAFETY: as above.
    let mut previous: [libc::sigaction; 3] = unsafe { std::mem::zeroed() };
    for (i, &signal) in SIGNALS.iter().enumerate() {
        // SAFETY: `ours` is a valid `sigaction`, and `previous[i]` a valid place for the old one.
        if unsafe { libc::sigaction(signal, &ours, &mut previous[i]) } != 0 {
            let failed = io::Error::last_os_error();
            restore(&previous[..i]);
            return Err(failed);
        }
    }
    Ok(previous)
}

/// Puts `previous` back for the first signals of [`SIGNALS`]. Cleanup: it never fails the caller.
fn restore(previous: &[libc::sigaction]) {
    for (&signal, old) in SIGNALS.iter().zip(previous) {
        // SAFETY: `old` is a `sigaction` this process read from the kernel.
        let put_back = unsafe { libc::sigaction(signal, old, std::ptr::null_mut()) };
        debug_assert_eq!(put_back, 0, "restoring the handler of signal {signal} failed");
    }
}

#[cfg(test)]
#[path = "interrupts_tests.rs"]
pub(super) mod interrupts_tests;
