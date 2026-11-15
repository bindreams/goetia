//! The deferral over real signals raised at this process. Each test puts a stand-in handler where the
//! process's own would be, so a re-raised signal lands there and never ends the test.

use std::cell::Cell;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;

use super::*;

/// The three signals, without their names.
fn signals() -> [libc::c_int; 3] {
    SIGNALS.map(|(signal, _)| signal)
}

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
/// The thread the stand-in handler ran on last (a `pthread_t`).
static SEEN_ON: AtomicUsize = AtomicUsize::new(0);

extern "C" fn stand_in(signal: libc::c_int) {
    // SAFETY: `pthread_self` has no preconditions.
    SEEN_ON.store(unsafe { libc::pthread_self() }, Ordering::SeqCst);
    SEEN.fetch_add(1, Ordering::SeqCst);
    SEEN_BITS.fetch_or(1 << signal, Ordering::SeqCst);
}

fn stand_in_address() -> libc::sighandler_t {
    stand_in as extern "C" fn(libc::c_int) as libc::sighandler_t
}

/// The handler a signal has now.
fn handler_of(signal: libc::c_int) -> libc::sighandler_t {
    let mut current = action(libc::SIG_DFL);
    // SAFETY: `current` is a valid place to read one into; a null new action only reads.
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
        let ours = action(stand_in_address());
        let mut before = [action(libc::SIG_DFL); 3];
        for (i, signal) in signals().into_iter().enumerate() {
            // SAFETY: `ours` is a valid `sigaction`, and `before[i]` a valid place for the old one.
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

    /// Whether the stand-in last ran on `thread`.
    fn ran_on(&self, thread: libc::pthread_t) -> bool {
        SEEN_ON.load(Ordering::SeqCst) == thread
    }

    /// Every one of the three signals is the stand-in's again.
    pub(in crate::backend::launchd::manager) fn assert_back(&self) {
        for signal in signals() {
            assert_eq!(handler_of(signal), stand_in_address(), "signal {signal}");
        }
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        restore(&self.before, ALL);
    }
}

fn raise(signal: libc::c_int) {
    // SAFETY: `raise` takes a valid signal number, and every one raised here has a handler.
    assert_eq!(unsafe { libc::raise(signal) }, 0, "raise {signal}");
}

/// Sets `signal` to be ignored, as `nohup` does SIGHUP. A [`StandIn`] taken before it puts the earlier
/// disposition back.
pub(in crate::backend::launchd::manager) fn ignore(signal: libc::c_int) {
    let ignoring = action(libc::SIG_IGN);
    // SAFETY: `ignoring` is a valid `sigaction`, and a null old action is allowed.
    let set = unsafe { libc::sigaction(signal, &ignoring, std::ptr::null_mut()) };
    assert_eq!(set, 0, "ignore signal {signal}");
}

/// Sends `signal` to this process, not to a thread: what a terminal or a service manager does, and the only
/// kind a kqueue has an event for.
pub(in crate::backend::launchd::manager) fn send_to_process(signal: libc::c_int) {
    // SAFETY: `kill` takes plain integers.
    assert_eq!(unsafe { libc::kill(libc::getpid(), signal) }, 0, "send signal {signal}");
}

/// Whether a read of `fd` would not block: a `poll` that waits for nothing.
fn readable(fd: RawFd) -> bool {
    let mut asked = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `asked` is one valid `pollfd`, and a zero timeout does not block.
    let ready = unsafe { libc::poll(&mut asked, 1, 0) };
    assert!(ready >= 0, "poll {fd}: {}", io::Error::last_os_error());
    ready == 1 && asked.revents & libc::POLLIN != 0
}

/// Whether `fd` is an open descriptor of this process.
fn is_open(fd: RawFd) -> bool {
    // SAFETY: `fcntl` with `F_GETFD` only reads the descriptor's flags.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().expect("an OS error")
}

fn set_errno(value: i32) {
    // SAFETY: `__error` is the calling thread's `errno`.
    unsafe { *libc::__error() = value };
}

/// Bits of [`RECORDED`] set by hand, cleared again when this drops, whatever the test left of them.
struct Planted(u32);

impl Planted {
    fn bits(mask: u32) -> Planted {
        RECORDED.fetch_or(mask, Ordering::SeqCst);
        Planted(mask)
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        claim(self.0);
    }
}

/// A handler stood in for as running: [`IN_FLIGHT`] counts it until this drops.
struct Running;

impl Running {
    fn enter() -> Running {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A thread joined when this drops, so that a test that fails does not leave it running into the next. It
/// is declared before whatever the thread waits for, which then drops first.
#[derive(Default)]
struct Joined(Option<JoinHandle<()>>);

impl Drop for Joined {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            _ = thread.join();
        }
    }
}

/// A signal that was ignored before the window stays ignored: the window does not take it over, so its
/// handler never runs, it is no interrupt and wakes nothing, it is not re-raised, and its disposition is
/// the ignored one throughout and after.
#[skuld::test]
fn an_ignored_signal_stays_ignored_and_is_not_taken_over() {
    let before = StandIn::install();
    ignore(libc::SIGHUP);
    let deferred = Deferred::begin().expect("begin");

    assert_eq!(handler_of(libc::SIGHUP), libc::SIG_IGN, "left alone");
    assert_eq!(
        handler_of(libc::SIGINT),
        record as extern "C" fn(libc::c_int) as libc::sighandler_t,
        "the others are taken over"
    );
    send_to_process(libc::SIGHUP);
    assert!(deferred.recorded().is_none(), "an ignored signal is no interrupt");
    assert!(!readable(deferred.wake()), "and wakes nothing");
    raise(libc::SIGINT);
    assert!(deferred.recorded().is_some(), "the others are still held");
    assert!(readable(deferred.wake()));

    drop(deferred);

    assert_eq!(handler_of(libc::SIGHUP), libc::SIG_IGN, "still ignored after");
    assert_eq!(before.seen(), 1, "only the interrupt that was held is re-raised");
    assert!(before.saw(libc::SIGINT) && !before.saw(libc::SIGHUP));
}

/// What a snapshot names is decided by the one read that made it: a signal that arrives after it is not in
/// it, and a signal the window did not take over is in none.
#[skuld::test]
fn a_snapshot_names_the_signals_recorded_when_it_was_taken() {
    let _before = StandIn::install();
    ignore(libc::SIGHUP);
    let deferred = Deferred::begin().expect("begin");
    assert!(deferred.recorded().is_none());

    raise(libc::SIGINT);
    let first = deferred.recorded().expect("SIGINT is recorded");
    raise(libc::SIGTERM);
    let second = deferred.recorded().expect("both are recorded");
    // The handler of a signal the window did not take over is never run by the system; this is the one
    // thing that could put its bit in the record.
    record(libc::SIGHUP);
    let third = deferred.recorded().expect("still both");

    assert_eq!(first.to_string(), "SIGINT");
    assert_eq!(second.to_string(), "SIGINT and SIGTERM");
    assert_eq!(third.to_string(), "SIGINT and SIGTERM", "SIGHUP was not taken over");
}

/// All three, as a sentence.
#[skuld::test]
fn a_snapshot_of_the_three_names_them_in_order() {
    let _before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    for signal in signals() {
        raise(signal);
    }

    assert_eq!(
        deferred.recorded().expect("recorded").to_string(),
        "SIGINT, SIGTERM and SIGHUP"
    );
}

/// One signal blocked in this thread's mask, put back when it drops.
pub(in crate::backend::launchd::manager) struct Blocking(libc::sigset_t);

impl Blocking {
    pub(in crate::backend::launchd::manager) fn signal(signal: libc::c_int) -> Blocking {
        // SAFETY: an all-zero `sigset_t` is a valid place for one; `sigemptyset` and `sigaddset` fill it
        // with a valid signal, and `pthread_sigmask` reads it and writes the old mask into `old`.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old),
                0,
                "block {signal}"
            );
            Blocking(old)
        }
    }
}

impl Drop for Blocking {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the mask `pthread_sigmask` gave back. Cleanup never panics.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
    }
}

/// A thread that blocks nothing and does nothing until it is dropped, for a signal sent to it by
/// [`Receiver::send`]: a signal that is delivered to a thread other than the one that opened the window.
/// It is started before the test blocks anything, as a thread inherits the mask of the one that starts it.
pub(in crate::backend::launchd::manager) struct Receiver {
    thread: libc::pthread_t,
    end: Option<Sender<()>>,
    joined: Option<JoinHandle<()>>,
}

impl Receiver {
    pub(in crate::backend::launchd::manager) fn start() -> Receiver {
        let (started, thread) = channel();
        let (end, ended) = channel::<()>();
        let joined = std::thread::spawn(move || {
            // SAFETY: `pthread_self` has no preconditions.
            started
                .send(unsafe { libc::pthread_self() })
                .expect("the test is waiting");
            // Ends when the sender is dropped.
            _ = ended.recv();
        });
        Receiver {
            thread: thread.recv().expect("the thread started"),
            end: Some(end),
            joined: Some(joined),
        }
    }

    /// Sends `signal` to this thread alone.
    pub(in crate::backend::launchd::manager) fn send(&self, signal: libc::c_int) {
        // SAFETY: the thread is alive, as it ends only when `self` drops.
        assert_eq!(
            unsafe { libc::pthread_kill(self.thread, signal) },
            0,
            "send signal {signal}"
        );
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        drop(self.end.take());
        if let Some(joined) = self.joined.take() {
            _ = joined.join();
        }
    }
}

/// An interrupt makes the wake readable, and nothing reads it: it stays readable, so every wait on every
/// queue that has it registered sees it.
#[skuld::test]
fn an_interrupt_makes_the_wake_readable_and_nothing_drains_it() {
    let _before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    assert!(!readable(deferred.wake()), "nothing recorded");

    raise(libc::SIGTERM);

    assert!(readable(deferred.wake()));
    assert!(deferred.recorded().is_some());
    assert!(readable(deferred.wake()), "asking drains nothing");
}

/// The wake is non-blocking and close-on-exec at both ends: a handler that finds the pipe full does not
/// block, and a program the run starts does not inherit it.
#[skuld::test]
fn the_wake_is_non_blocking_and_close_on_exec() {
    let _before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    let ends = [deferred.wake(), WAKE.load(Ordering::SeqCst)];

    for end in ends {
        // SAFETY: `fcntl` reads the flags of a descriptor that is open.
        let (status, descriptor) = unsafe { (libc::fcntl(end, libc::F_GETFL), libc::fcntl(end, libc::F_GETFD)) };
        assert!(status & libc::O_NONBLOCK != 0, "{end} is non-blocking");
        assert!(descriptor & libc::FD_CLOEXEC != 0, "{end} is close-on-exec");
    }
}

/// A pipe that is full still holds an interrupt, and still reads as one: the handler's write fails, it is
/// not waited on, and it leaves the `errno` of the code it interrupted as it was.
#[skuld::test]
fn a_full_pipe_loses_no_interrupt_and_the_handler_leaves_errno_alone() {
    let _before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    let write = WAKE.load(Ordering::SeqCst);
    let byte = 0u8;
    // Ends when the write fails, which a non-blocking pipe does when it is full.
    // SAFETY: `write` is the open write end, and `byte` is one valid byte.
    while unsafe { libc::write(write, (&byte as *const u8).cast(), 1) } == 1 {}
    assert_eq!(errno(), libc::EAGAIN, "the pipe is full");

    set_errno(libc::ENOENT);
    record(libc::SIGINT);

    assert_eq!(errno(), libc::ENOENT, "the handler's failed write left errno alone");
    assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), 0, "and counted itself out");
    assert_eq!(deferred.recorded().expect("recorded").to_string(), "SIGINT");
    assert!(readable(deferred.wake()));
}

/// The last drop un-publishes the write end and then waits for a handler that is running before it closes
/// the pipe, so a handler never writes to a closed descriptor. A running handler is stood in for by a count
/// that the test holds; the drop is observed at its un-publishing, which is a fact of the state and not a
/// wait of a chosen length.
#[skuld::test]
fn the_last_drop_waits_for_a_running_handler_before_it_closes_the_pipe() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    let wake = deferred.wake();
    let mut closing = Joined::default();
    let running = Running::enter();

    closing.0 = Some(std::thread::spawn(move || drop(deferred)));
    while WAKE.load(Ordering::SeqCst) != -1 {
        std::thread::yield_now();
    }
    assert!(is_open(wake), "a handler is running, so the pipe is still open");

    drop(running);
    drop(closing);
    before.assert_back();
}

/// What [`IN_FLIGHT`] was when the probe ran.
static COUNTED_BY_PROBE: AtomicUsize = AtomicUsize::new(usize::MAX);

extern "C" fn probe(_: libc::c_int) {
    COUNTED_BY_PROBE.store(IN_FLIGHT.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// A handler is counted in only for the load of the pipe and the write, so a close never waits on what a
/// handler does after: here the previous handler, which the handler raises the signal to once the window is
/// closed, and which may never return.
#[skuld::test]
fn a_handler_is_not_counted_in_when_it_raises_to_the_previous_handler() {
    let _before = StandIn::install();
    let probing = action(probe as extern "C" fn(libc::c_int) as libc::sighandler_t);
    // SAFETY: `probing` is a valid `sigaction`, and a null old action is allowed; the stand-in's drop puts
    // the earlier handler back.
    let set = unsafe { libc::sigaction(libc::SIGTERM, &probing, std::ptr::null_mut()) };
    assert_eq!(set, 0, "install the probe");
    COUNTED_BY_PROBE.store(usize::MAX, Ordering::SeqCst);

    record(libc::SIGTERM);

    assert_eq!(
        COUNTED_BY_PROBE.load(Ordering::SeqCst),
        0,
        "the probe ran, and nothing was counted in"
    );
}

/// The re-raised signal is sent to the process, not to the thread that closes the window: a thread that
/// blocks it would hold it pending for as long as it blocks, and deliver it to itself only once it lets go,
/// while another thread that does not block it takes it at once. The closing thread lets go of the signal
/// before the test looks, so that a signal held pending on it would be seen, and be seen on that thread.
#[skuld::test]
fn a_re_raised_signal_reaches_the_previous_handler_though_the_closing_thread_blocks_it() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    raise(libc::SIGINT);

    let closing = std::thread::spawn(move || {
        let _blocked = Blocking::signal(libc::SIGINT);
        drop(deferred);
        // SAFETY: `pthread_self` has no preconditions.
        unsafe { libc::pthread_self() }
    });
    // The thread is over, its block lifted: a signal it held pending has been delivered to it by now.
    let closer = closing.join().expect("the closing thread");
    // A signal taken by another thread is delivered on that thread, asynchronously, so it is awaited, under
    // no bound; one the closer held is seen already.
    while before.seen() == 0 {
        std::thread::yield_now();
    }

    assert!(before.saw(libc::SIGINT));
    assert!(
        !before.ran_on(closer),
        "the signal was held by the thread that closed the window"
    );
}

/// A window whose pipe cannot be made is an `Err` and installs nothing: no handler is ours, nothing is
/// published, and no window is open.
#[skuld::test]
fn a_begin_that_cannot_make_the_pipe_installs_nothing() {
    let before = StandIn::install();
    let none = test_hook::steps(0);

    let refused = Deferred::begin().err().expect("no pipe, no window");

    drop(none);
    assert!(refused.to_string().contains("interrupts"), "{refused}");
    assert!(!OPEN.load(Ordering::SeqCst));
    assert_eq!(WAKE.load(Ordering::SeqCst), -1, "nothing is published");
    before.assert_back();
    assert_eq!(SHARED.lock().unwrap_or_else(PoisonError::into_inner).holders, 0);
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

/// The window says whether a signal has arrived in it, without taking the signal: it is still re-raised
/// when the window ends.
#[skuld::test]
fn the_window_says_whether_a_signal_arrived_in_it() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    assert!(deferred.recorded().is_none());

    raise(libc::SIGHUP);

    assert!(deferred.recorded().is_some());
    assert!(deferred.recorded().is_some(), "asking does not take it");
    drop(deferred);
    assert_eq!(before.seen(), 1, "re-raised once the window is over");
}

/// Every one of the three is held, and each is re-raised as itself.
#[skuld::test]
fn int_term_and_hup_are_each_held_and_re_raised() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");

    for signal in signals() {
        raise(signal);
    }

    assert_eq!(before.seen(), 0);
    drop(deferred);
    assert_eq!(before.seen(), 3);
    for signal in signals() {
        assert!(before.saw(signal), "signal {signal}");
    }
}

#[skuld::test]
fn the_previous_handler_is_restored() {
    let before = StandIn::install();
    let deferred = Deferred::begin().expect("begin");
    for signal in signals() {
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
    for signal in signals() {
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
    let _planted = Planted::bits(0b101);

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
/// install is refused for the second signal (the pipe is the first step, the first handler the second), and
/// a bit a handler recorded is there when it fails.
#[skuld::test]
fn a_begin_whose_install_fails_part_way_closes_the_window_and_re_raises_what_was_recorded() {
    let before = StandIn::install();
    let term = SIGNALS
        .iter()
        .position(|&(signal, _)| signal == libc::SIGTERM)
        .expect("SIGTERM is deferred");
    let _planted = Planted::bits(1 << term);
    let after_one = test_hook::steps(2);

    let refused = Deferred::begin().err().expect("the second install is refused");

    drop(after_one);
    assert!(refused.to_string().contains("interrupts"), "{refused}");
    assert!(!OPEN.load(Ordering::SeqCst), "the window is closed");
    assert_eq!(WAKE.load(Ordering::SeqCst), -1, "the pipe is no longer published");
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
