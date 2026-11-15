//! The watch joined to launchd's interrupt window: a launch's registration, and the waits that end on an
//! interrupt. The watch's own behaviour is tested beside it, in
//! [`process_watch_tests`](crate::backend::process_watch::process_watch_tests).

use super::super::interrupts::interrupts_tests::{Blocking, Receiver, StandIn, ignore, mark_thread, send_to_process};
use super::*;
use crate::backend::process_watch::process_watch_tests::{Held, poll, queue_of, woken_by};

/// An interruptible watch, which holds the window an interrupt is recorded in. The stand-in takes the
/// signals that are re-raised when the window closes, and the turn that keeps another test's dispositions
/// out of this.
fn interruptible() -> (StandIn, Watch) {
    let stand_in = StandIn::install();
    let watch = Watch::new_interruptible(&Deferred::begin().expect("begin the window")).expect("a kqueue");
    (stand_in, watch)
}

/// `pid` registered as a launch, which must be alive.
fn launched(watch: &Watch, pid: u32) -> Registered {
    match watch.add_launch(pid).expect("add_launch") {
        Added::Watching(registered) => registered,
        Added::Gone => panic!("process {pid} is gone, and it was held alive"),
    }
}

fn raise_interrupt() {
    // SAFETY: `raise` takes a valid signal number, and the window's handler is in place.
    assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
}

/// The first exec of a launch's pid is reported, and its exit is kept for the wait that follows: exec and
/// exit both happened before anything asked.
#[skuld::test]
fn a_launch_reports_its_first_exec_and_keeps_its_exit() {
    let (_stand_in, watch) = interruptible();
    let mut child = Held::execing(3);
    let registered = launched(&watch, child.pid());
    child.finish();

    assert!(matches!(registered.first_exec().expect("first_exec"), Woke::Exec));

    assert!(
        matches!(registered.exit().expect("exit"), Woke::Exited(Some(status)) if status == 3 << 8),
        "the exit was kept"
    );
}

/// A launch that exits without an exec is reported as its exit.
#[skuld::test]
fn a_launch_that_exits_without_an_exec_is_its_exit() {
    let (_stand_in, watch) = interruptible();
    let mut child = Held::new(2);
    let registered = launched(&watch, child.pid());
    child.finish();

    assert!(matches!(registered.first_exec().expect("first_exec"), Woke::Exited(Some(status)) if status == 2 << 8));
}

/// A registration made for a launch is still a registration of one pid, deleted on drop: the exec and the
/// exit that follow queue nothing.
#[skuld::test]
fn a_launch_registration_dropped_early_queues_nothing() {
    let (_stand_in, watch) = interruptible();
    let mut child = Held::execing(0);
    drop(launched(&watch, child.pid()));

    child.finish();

    assert_eq!(poll(queue_of(&watch)), 0);
}

/// An interrupt recorded before the wait is seen before it blocks, whatever the pid does: neither wait
/// returns the pid's state, and the pid is still alive.
#[skuld::test]
fn an_interrupt_recorded_before_a_wait_ends_it_before_it_blocks() {
    let (_stand_in, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());
    raise_interrupt();

    assert!(matches!(
        registered.first_exec().expect("first_exec"),
        Woke::Interrupted(_)
    ));
    assert!(matches!(registered.exit().expect("exit"), Woke::Interrupted(_)));
}

/// An interrupt wins over an exec or an exit that another read kept: a wait that reports the pid's state
/// while an interrupt is known would have `run` resume or follow a launch after it. The poll reads and keeps
/// the exec and the exit before the interrupt is recorded, so both are waiting when the wait begins.
#[skuld::test]
fn an_interrupt_wins_over_an_exec_and_an_exit_that_were_kept() {
    let (_stand_in, watch) = interruptible();
    let mut child = Held::execing(3);
    let registered = launched(&watch, child.pid());
    child.finish();
    assert!(
        registered.exit_kept().expect("poll").is_some(),
        "the exec and the exit are kept"
    );
    raise_interrupt();

    assert!(matches!(
        registered.first_exec().expect("first_exec"),
        Woke::Interrupted(_)
    ));
    assert!(matches!(registered.exit().expect("exit"), Woke::Interrupted(_)));
}

/// An interrupt that arrives while the wait blocks wakes it. A second process sends SIGINT to this one
/// once its stdin closes, which a thread does as the wait begins: whether the signal lands before the wait
/// blocks (recorded) or after (the pipe's wake), the wait ends.
#[skuld::test]
fn an_interrupt_during_a_wait_wakes_it() {
    let (_stand_in, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());
    let mut sender = Held::running("read _; kill -INT $PPID");
    let sending = std::thread::spawn(move || sender.finish());

    let woke = registered.exit().expect("exit");

    assert!(matches!(woke, Woke::Interrupted(_)));
    sending.join().expect("the sender's thread");
}

/// A signal the opening thread blocks is the window's all the same: the handler is a process-wide one, and a
/// signal sent to the process goes to a thread that does not block it. Sent to such a thread, it is recorded
/// there and ends the wait of this one. (The receiver is made before anything is blocked, as a thread
/// inherits its starter's mask.)
#[skuld::test]
fn a_signal_blocked_on_the_opening_thread_but_delivered_to_another_ends_a_wait_as_interrupted() {
    let receiver = Receiver::start();
    let stand_in = StandIn::install();
    let blocking = Blocking::signal(libc::SIGTERM);
    let window = Deferred::begin().expect("begin the window");
    let watch = Watch::new_interruptible(&window).expect("a kqueue");
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());

    receiver.send(libc::SIGTERM);
    let woke = registered.exit().expect("exit");

    let Woke::Interrupted(interrupts) = woke else {
        panic!("the wait ended by the child's exit, which is alive");
    };
    assert_eq!(interrupts.to_string(), "SIGTERM");
    assert_eq!(stand_in.seen(), 0, "held back, not delivered to the earlier handler");

    // The last holder is dropped while this thread still blocks the signal, so that the window closes on a
    // thread that cannot take it. With a plain `raise` the signal would stay pending here until the block
    // lifts, and be delivered to this thread then, which is marked.
    let _marked = mark_thread();
    drop(watch);
    drop(window);
    drop(blocking);
    // Sent to the process, it is delivered to another thread, asynchronously, so it is awaited, under no
    // bound; one held pending here was delivered as the block lifted, and is counted already.
    while stand_in.seen() == 0 {
        std::thread::yield_now();
    }
    assert!(stand_in.saw(libc::SIGTERM), "re-raised to the earlier handler");
    assert!(
        !stand_in.ran_on_marked(),
        "the signal was held by the thread that closed the window"
    );
}

/// A signal that was ignored before the window is none of its interrupts: its handler never runs, so it
/// neither wakes the wait nor ends it. The wait ends with the child's exit, with the signal sent to the
/// process while it blocks or before.
#[skuld::test]
fn an_ignored_signal_neither_wakes_nor_interrupts_a_wait() {
    let stand_in = StandIn::install();
    ignore(libc::SIGHUP);
    let window = Deferred::begin().expect("begin the window");
    let watch = Watch::new_interruptible(&window).expect("a kqueue");
    let mut child = Held::new(0);
    let registered = launched(&watch, child.pid());
    let ending = std::thread::spawn(move || {
        send_to_process(libc::SIGHUP);
        child.finish()
    });

    let woke = registered.exit().expect("exit");

    assert!(matches!(woke, Woke::Exited(Some(0))), "the child's own exit ended it");
    assert_eq!(ending.join().expect("the child's thread"), Some(0));
    assert_eq!(stand_in.seen(), 0);
}

/// A recorded interrupt is an event on every queue that has the window's wake registered, the ones made
/// before it and the ones made after: nothing drains the pipe, and each queue has its own event.
#[skuld::test]
fn a_recorded_interrupt_is_an_event_of_every_queue_of_the_window() {
    let _stand_in = StandIn::install();
    let window = Deferred::begin().expect("begin the window");
    let first = Watch::new_interruptible(&window).expect("a first kqueue");
    let second = Watch::new_interruptible(&window).expect("a second kqueue");
    assert!(!woken_by(queue_of(&first), window.wake()), "nothing recorded yet");

    raise_interrupt();
    let third = Watch::new_interruptible(&window).expect("a third kqueue");

    for (name, watch) in [("first", &first), ("second", &second), ("third", &third)] {
        assert!(woken_by(queue_of(watch), window.wake()), "the {name} queue");
    }
}

/// A launch's registration that has left the window does not hold it open: with the watch gone, the
/// signals are the earlier handler's again, though the registration is alive.
#[skuld::test]
fn a_registration_that_left_the_window_does_not_keep_it_open() {
    let (stand_in, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid()).leave_window();

    drop(watch);

    stand_in.assert_back();
    drop(registered);
}

/// A launch's registration holds the window, as its watch does: with the watch dropped, an interrupt is
/// still recorded and not delivered to the earlier handler.
#[skuld::test]
fn a_launch_registration_holds_the_window_of_its_watch() {
    let (stand_in, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());
    drop(watch);

    raise_interrupt();

    assert_eq!(stand_in.seen(), 0, "held back, not delivered to the earlier handler");
    assert!(matches!(registered.exit().expect("exit"), Woke::Interrupted(_)));
}

/// A watch reads the record of its own window, and a watch that is not interruptible has none.
#[skuld::test]
fn a_watch_reads_the_record_of_its_window() {
    let (_stand_in, watch) = interruptible();
    assert_eq!(watch.recorded(), None);

    raise_interrupt();

    assert_eq!(watch.recorded().expect("recorded").to_string(), "SIGINT");
    assert_eq!(Watch::new().expect("a kqueue").recorded(), None, "no window, no record");
}

/// A poll for the exit keeps the exec it reads for the wait for the exec.
#[skuld::test]
fn a_poll_keeps_the_exec_for_the_wait_for_it() {
    let (_stand_in, watch) = interruptible();
    let mut child = Held::execing(3);
    let registered = launched(&watch, child.pid());
    child.finish();

    assert!(registered.exit_kept().expect("poll").is_some());

    assert!(matches!(registered.first_exec().expect("first_exec"), Woke::Exec));
}
