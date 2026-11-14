//! The deferral over real signals raised at this process. Each test puts a stand-in handler where the
//! process's own would be, so a re-raised signal lands there and never ends the test.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::*;

/// Dispositions belong to the process, so the tests that change them take turns, whatever runs them.
static SERIAL: Mutex<()> = Mutex::new(());

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
    _serial: std::sync::MutexGuard<'static, ()>,
    before: [libc::sigaction; 3],
}

impl StandIn {
    pub(in crate::backend::launchd::manager) fn install() -> StandIn {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
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
