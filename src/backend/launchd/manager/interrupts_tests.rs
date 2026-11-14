//! The deferral over real signals raised at this process. Each test puts a stand-in handler where the
//! process's own would be, so a re-raised signal lands there and never ends the test.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use super::*;

/// Dispositions belong to the process, so every test that changes them, `begin_run`'s included, takes
/// turns, whatever runs them.
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    /// Whether this thread holds [`SERIAL`] already, so that a test that holds it through a [`StandIn`]
    /// can call what takes it too.
    static HOLDS: Cell<bool> = const { Cell::new(false) };
}

/// The turn a test takes before it changes the process's signal dispositions.
pub(in crate::backend::launchd::manager) struct Serial(Option<MutexGuard<'static, ()>>);

/// Waits for the turn, unless this thread has it.
pub(in crate::backend::launchd::manager) fn serial() -> Serial {
    if HOLDS.get() {
        return Serial(None);
    }
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    HOLDS.set(true);
    Serial(Some(guard))
}

impl Drop for Serial {
    fn drop(&mut self) {
        if self.0.is_some() {
            HOLDS.set(false);
        }
    }
}

/// How many signals the stand-in handler has seen, and which (bit `n` for signal `n`).
static SEEN: AtomicUsize = AtomicUsize::new(0);
static SEEN_BITS: AtomicU32 = AtomicU32::new(0);

extern "C" fn stand_in(signal: libc::c_int) {
    SEEN.fetch_add(1, Ordering::SeqCst);
    SEEN_BITS.fetch_or(1 << signal, Ordering::SeqCst);
}

fn stand_in_address() -> libc::sighandler_t {
    stand_in as extern "C" fn(libc::c_int) as libc::sighandler_t
}

/// The handler a signal has now.
fn handler_of(signal: libc::c_int) -> libc::sighandler_t {
    // SAFETY: an all-zero `sigaction` is a valid place to read one into; a null new action only reads.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    let read = unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) };
    assert_eq!(read, 0, "read the handler of signal {signal}");
    current.sa_sigaction
}

/// How many signals the stand-in has seen since it was installed.
pub(in crate::backend::launchd::manager) fn seen() -> usize {
    SEEN.load(Ordering::SeqCst)
}

/// The stand-in installed for the three signals, and what they had before it, put back when it drops.
pub(in crate::backend::launchd::manager) struct StandIn {
    _serial: Serial,
    before: [libc::sigaction; 3],
}

impl StandIn {
    pub(in crate::backend::launchd::manager) fn install() -> StandIn {
        let serial = serial();
        SEEN.store(0, Ordering::SeqCst);
        SEEN_BITS.store(0, Ordering::SeqCst);
        // SAFETY: an all-zero `sigaction` is valid; the mask is emptied before use.
        let mut ours: libc::sigaction = unsafe { std::mem::zeroed() };
        ours.sa_sigaction = stand_in_address();
        unsafe { libc::sigemptyset(&mut ours.sa_mask) };
        let mut before: [libc::sigaction; 3] = unsafe { std::mem::zeroed() };
        for (i, &signal) in SIGNALS.iter().enumerate() {
            let set = unsafe { libc::sigaction(signal, &ours, &mut before[i]) };
            assert_eq!(set, 0, "install the stand-in for signal {signal}");
        }
        StandIn {
            _serial: serial,
            before,
        }
    }

    pub(in crate::backend::launchd::manager) fn seen(&self) -> usize {
        seen()
    }

    pub(in crate::backend::launchd::manager) fn saw(&self, signal: libc::c_int) -> bool {
        SEEN_BITS.load(Ordering::SeqCst) & (1 << signal) != 0
    }

    /// Every one of the three signals is the stand-in's again.
    pub(in crate::backend::launchd::manager) fn assert_back(&self) {
        for signal in SIGNALS {
            assert_eq!(handler_of(signal), stand_in_address(), "signal {signal}");
        }
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        restore(&self.before);
    }
}

fn raise(signal: libc::c_int) {
    // SAFETY: `raise` takes a valid signal number, and every one raised here has a handler.
    assert_eq!(unsafe { libc::raise(signal) }, 0, "raise {signal}");
}

/// An interrupt that arrives in the window is held until it ends, and then reaches the handler that was
/// there before, once.
#[skuld::test]
fn a_signal_during_the_window_is_recorded_and_re_raised_after_it() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");

    raise(libc::SIGINT);

    assert_eq!(before.seen(), 0, "the window holds the signal back");
    drop(deferred);
    assert_eq!(before.seen(), 1, "it is re-raised once the window is over");
    assert!(before.saw(libc::SIGINT));
}

/// Every one of the three is held, and each is re-raised as itself.
#[skuld::test]
fn int_term_and_hup_are_each_held_and_re_raised() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");

    for signal in SIGNALS {
        raise(signal);
    }

    assert_eq!(before.seen(), 0);
    drop(deferred);
    assert_eq!(before.seen(), 3);
    for signal in SIGNALS {
        assert!(before.saw(signal), "signal {signal}");
    }
}

#[skuld::test]
fn the_previous_handler_is_restored() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    for signal in SIGNALS {
        assert_eq!(
            handler_of(signal),
            record as extern "C" fn(libc::c_int) as libc::sighandler_t
        );
    }

    drop(deferred);

    before.assert_back();
}

/// Outside the window a signal goes to the handler there, as it did before `begin` and as it does after.
#[skuld::test]
fn a_signal_before_and_after_is_untouched() {
    let before = StandIn::install();

    raise(libc::SIGTERM);
    assert_eq!(before.seen(), 1, "before");
    drop(Deferred::begin().expect("begin"));
    assert_eq!(before.seen(), 1, "an empty window re-raises nothing");
    raise(libc::SIGHUP);

    assert_eq!(before.seen(), 2, "after");
    assert!(before.saw(libc::SIGTERM) && before.saw(libc::SIGHUP));
}

#[skuld::test]
fn no_signal_no_re_raise() {
    let before = StandIn::install();

    drop(Deferred::begin().expect("begin"));

    assert_eq!(before.seen(), 0);
}

/// A window opened while another is live joins it: the handlers stay goetia's until the last is dropped,
/// the signal recorded in between is re-raised then, and the original dispositions are the ones put back
/// (the second `begin` did not save goetia's own as the previous).
#[skuld::test]
fn two_overlapping_begin_runs_restore_the_signal_handlers() {
    let before = StandIn::install();
    let first = Deferred::begin().expect("first begin");
    let second = Deferred::begin().expect("second begin");

    drop(first);
    for signal in SIGNALS {
        assert_eq!(
            handler_of(signal),
            record as extern "C" fn(libc::c_int) as libc::sighandler_t,
            "the first drop leaves the handlers ours: signal {signal}"
        );
    }
    raise(libc::SIGTERM);
    assert_eq!(before.seen(), 0, "still held with one window live");

    drop(second);

    assert_eq!(before.seen(), 1);
    assert!(before.saw(libc::SIGTERM));
    before.assert_back();
}

/// Each recorded bit is claimed once: whoever takes it re-raises it, so a signal is never lost between the
/// handler and the drop, and never raised twice.
#[skuld::test]
fn a_recorded_signal_is_claimed_once() {
    let _before = StandIn::install();
    RECORDED.store(0b101, Ordering::SeqCst);

    assert_eq!(claim(0b001), 0b001);
    assert_eq!(claim(0b001), 0, "taken already");
    assert_eq!(claim(ALL), 0b100, "what is left");
    assert_eq!(RECORDED.load(Ordering::SeqCst), 0);
}

/// A handler that was running while the last window was dropped records its signal after the drop has
/// claimed what it saw. It finds the window closed, claims its own bit, and raises it to the handler that
/// is back in place: the signal is delivered once, not swallowed.
#[skuld::test]
fn a_handler_that_finishes_after_the_window_closed_re_raises_its_own_signal() {
    let before = StandIn::install();
    // What `Drop` leaves: the previous handlers back (here the stand-in), the window closed, nothing recorded.
    OPEN.store(false, Ordering::SeqCst);

    record(libc::SIGTERM);

    assert_eq!(before.seen(), 1);
    assert!(before.saw(libc::SIGTERM));
    assert_eq!(
        RECORDED.load(Ordering::SeqCst),
        0,
        "claimed, so not left for a drop that is over"
    );
}

/// `begin` opens the window before it installs anything, so a signal that a handler records while it
/// installs is held, and a `begin` whose install fails closes the window as the last drop does: the
/// handlers it changed are back, and what was recorded meanwhile is re-raised to them once. Here the
/// install is refused for the second signal, and a bit a handler recorded is there when it fails.
#[skuld::test]
fn a_begin_whose_install_fails_part_way_closes_the_window_and_re_raises_what_was_recorded() {
    let before = StandIn::install();
    let term = SIGNALS
        .iter()
        .position(|&s| s == libc::SIGTERM)
        .expect("SIGTERM is deferred");
    RECORDED.store(1 << term, Ordering::SeqCst);
    let after_one = test_hook::installs(1);

    let refused = Deferred::begin().err().expect("the second install is refused");

    drop(after_one);
    assert!(refused.to_string().contains("interrupts"), "{refused}");
    assert!(!OPEN.load(Ordering::SeqCst), "the window is closed");
    assert_eq!(RECORDED.load(Ordering::SeqCst), 0, "what was recorded was claimed");
    assert_eq!(before.seen(), 1, "and re-raised once");
    assert!(before.saw(libc::SIGTERM));
    before.assert_back();
    assert_eq!(SHARED.lock().unwrap_or_else(PoisonError::into_inner).holders, 0);
}

/// The window is open once `begin` has installed a handler, so a signal that arrives with only some of
/// them installed is recorded, never raised into our own handler.
#[skuld::test]
fn the_window_is_open_while_the_handlers_are_installed() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");

    assert!(OPEN.load(Ordering::SeqCst));
    raise(libc::SIGHUP);
    assert_eq!(before.seen(), 0);

    drop(deferred);
    assert_eq!(before.seen(), 1);
    assert!(!OPEN.load(Ordering::SeqCst));
}

/// Windows that follow one another each save and restore: nothing a first one recorded is re-raised by
/// the second.
#[skuld::test]
fn a_window_after_a_window_starts_clean() {
    let before = StandIn::install();
    let first = Deferred::begin().expect("begin");
    raise(libc::SIGINT);
    drop(first);
    assert_eq!(before.seen(), 1);

    drop(Deferred::begin().expect("begin again"));

    assert_eq!(
        before.seen(),
        1,
        "the signal is re-raised once, not by every later window"
    );
    before.assert_back();
}
