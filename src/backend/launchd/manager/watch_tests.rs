//! The watch over real child processes, each held alive by an open stdin until the test lets it go.
//! Nothing here needs root, launchd or a file outside the process.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use super::super::interrupts::interrupts_tests::StandIn;
use super::*;
use crate::manager::Budget;

/// A child that exits `code` once its stdin closes: alive, and so watchable, until the test says. The
/// stop tests use it as the run a task's job has.
pub(in crate::backend::launchd::manager) struct Held {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Held {
    pub(in crate::backend::launchd::manager) fn new(code: i32) -> Held {
        Held::running(&format!("read _; exit {code}"))
    }

    /// A child that ends itself with `signal` once its stdin closes.
    pub(in crate::backend::launchd::manager) fn signalled(signal: i32) -> Held {
        Held::running(&format!("read _; kill -{signal} $$"))
    }

    /// A child that, once its stdin closes, execs a program that exits `code`.
    pub(in crate::backend::launchd::manager) fn execing(code: i32) -> Held {
        Held::running(&format!("read _; exec /bin/sh -c 'exit {code}'"))
    }

    /// The child, stopped as a held launch is: its state reads stopped from when this returns until it is
    /// continued (`finish` continues it).
    pub(in crate::backend::launchd::manager) fn stopped(self) -> Held {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: `pid` is this process's own live child; `kill` and `waitpid` take plain integers and
        // `waitpid` a valid place for the status.
        unsafe {
            assert_eq!(libc::kill(pid, libc::SIGSTOP), 0, "stop the child");
            let mut status = 0;
            assert_eq!(
                libc::waitpid(pid, &mut status, libc::WUNTRACED),
                pid,
                "wait for the stop"
            );
            assert!(libc::WIFSTOPPED(status), "the child stopped");
        }
        self
    }

    /// Answers the child's first `read` with a line and leaves its stdin open: a script that reads again
    /// (a program it execs) goes on blocking until `release`.
    pub(in crate::backend::launchd::manager) fn advance(&mut self) {
        let stdin = self.stdin.as_mut().expect("the child's stdin is open");
        stdin.write_all(b"\n").expect("write a line to the child");
        stdin.flush().expect("flush the child's stdin");
    }

    /// Closes the child's stdin, so that it goes on and ends, and returns without waiting for it.
    pub(in crate::backend::launchd::manager) fn release(&mut self) {
        drop(self.stdin.take());
    }

    /// [`Self::release`], then waits for the child to have exited and leaves it unreaped: a zombie, until
    /// `finish` or the drop reaps it.
    pub(in crate::backend::launchd::manager) fn exit_unreaped(&mut self) {
        self.release();
        // SAFETY: an all-zero `siginfo_t` is a valid place for `waitid` to fill, and the pid is this
        // process's own child. `WNOWAIT` leaves the child waitable.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let waited = libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            );
            assert_eq!(waited, 0, "wait for the child to exit");
        }
    }

    /// A child whose program is named `xpcproxy`, as the process of a launch is until it execs: a copy of
    /// `/bin/bash` in `dir`, which must outlive it, running `script`. Not `/bin/sh`, which on macOS is a
    /// stub that execs the selected shell, so the process would stop being `xpcproxy` at once.
    pub(in crate::backend::launchd::manager) fn at_xpcproxy(dir: &Path, script: &str) -> Held {
        let copy = dir.join("xpcproxy");
        std::fs::copy("/bin/bash", &copy).expect("copy /bin/bash");
        Held::spawned(Command::new(&copy).args(["-c", &format!("echo up; {script}")]))
    }

    fn running(script: &str) -> Held {
        Held::spawned(Command::new("/bin/sh").args(["-c", &format!("echo up; {script}")]))
    }

    /// Spawns `command`, whose script says `up` first, and returns once it has: the exec that started the
    /// child is then over. Without this its event can reach a registration made right after the spawn
    /// returns (seen on macOS CI), and a test that waits for the child's own exec would race it.
    fn spawned(command: &mut Command) -> Held {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the child");
        let mut said = String::new();
        BufReader::new(child.stdout.take().expect("the child's stdout"))
            .read_line(&mut said)
            .expect("read the child's first line");
        assert_eq!(said, "up\n", "the child's first line");
        let stdin = child.stdin.take();
        Held { child, stdin }
    }

    pub(in crate::backend::launchd::manager) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Continues the child (a no-op for one that is not stopped), lets it go and reaps it, so its exit has
    /// happened (and its event been queued) when this returns. Its exit code.
    pub(in crate::backend::launchd::manager) fn finish(&mut self) -> Option<i32> {
        if self.stdin.take().is_some() {
            // SAFETY: `kill` takes plain integers; the child is this process's own and not yet reaped, as
            // its stdin is still held.
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGCONT) };
        }
        self.child.wait().expect("wait for the child").code()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        drop(self.stdin.take());
        // Cleanup never panics: a child that is gone already is what is wanted.
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

/// How long a test waits for an exit event that is due before it calls the event lost: the failure bound
/// of an external event, surfaced as a test failure. A passing test never waits for it.
fn failure_bound() -> Option<Deadline> {
    Some(Budget::Bounded(Duration::from_secs(60)).start())
}

fn watch() -> Watch {
    Watch::new().expect("a kqueue")
}

/// `pid` registered on `watch`, which must be alive.
fn registered(watch: &Watch, pid: u32) -> Registered {
    match watch.add(pid).expect("add") {
        Added::Watching(registered) => registered,
        Added::Gone => panic!("process {pid} is gone, and it was held alive"),
    }
}

/// How many events `queue` holds now, which it gives up: a poll that blocks for none.
fn poll(queue: &Kqueue) -> usize {
    let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    queue.kevent(&[], &mut [blank(); 8], Some(now)).expect("poll the queue")
}

/// [`poll`] of the queue `registered` is on.
pub(in crate::backend::launchd::manager) fn pending(registered: &Registered) -> usize {
    poll(&registered.shared.queue)
}

/// A child that exits while nobody is waiting still has its status reported: the event is held by the
/// queue from the moment of the registration, which is made while the child lives.
#[skuld::test]
fn watch_reports_a_childs_exit_status_after_it_ended_quickly() {
    let watch = watch();
    let mut child = Held::new(3);
    let registered = registered(&watch, child.pid());

    assert_eq!(child.finish(), Some(3));

    assert_eq!(registered.wait(failure_bound()).expect("wait"), Some(3 << 8));
}

#[skuld::test]
fn one_kqueue_serves_two_pids_in_turn() {
    let watch = watch();
    for code in [3, 5] {
        let mut child = Held::new(code);
        let registered = registered(&watch, child.pid());
        child.finish();
        assert_eq!(registered.wait(failure_bound()).expect("wait"), Some(code << 8));
    }
}

/// A signalled process reports the signal in the status.
#[skuld::test]
fn a_killed_child_reports_its_signal() {
    let watch = watch();
    let mut child = Held::new(0);
    let registered = registered(&watch, child.pid());
    child.child.kill().expect("kill the child");
    child.child.wait().expect("reap it");

    let status = registered.wait(failure_bound()).expect("wait").expect("an exit");

    assert_eq!(
        crate::backend::launchd::state::exit_of(status),
        crate::manager::Exit::Signal(9)
    );
}

#[skuld::test]
fn a_gone_pid_is_gone() {
    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn true");
    let pid = child.id();
    child.wait().expect("reap it");

    assert!(matches!(watch().add(pid).expect("add"), Added::Gone));
}

#[skuld::test]
fn wait_with_an_expired_deadline_is_none() {
    let watch = watch();
    let child = Held::new(0);
    let registered = registered(&watch, child.pid());

    assert_eq!(registered.wait(Some(Budget::Immediate.start())).expect("wait"), None);
}

/// An exit that is already queued is reported past a deadline that has expired, as a poll.
#[skuld::test]
fn an_exit_already_queued_is_reported_under_an_expired_deadline() {
    let watch = watch();
    let mut child = Held::new(2);
    let registered = registered(&watch, child.pid());
    child.finish();

    assert_eq!(
        registered.wait(Some(Budget::Immediate.start())).expect("wait"),
        Some(2 << 8)
    );
}

#[skuld::test]
fn a_watch_that_cannot_be_made_is_an_error() {
    let _none = test_hook::queues(0);

    let e = Watch::new().err().expect("no kqueue may be made");

    assert!(e.to_string().contains("kqueue"), "{e}");
}

// A registration that is not waited on ================================================================================

/// A registration dropped after its pid exited, with the exit never read, leaves no event: a later
/// registration of a reused pid number would otherwise be handed this process's exit.
#[skuld::test]
fn a_registration_dropped_after_its_pid_exited_leaves_nothing_queued() {
    let watch = watch();
    let mut child = Held::new(4);
    let registration = registered(&watch, child.pid());
    child.finish();

    drop(registration);

    assert_eq!(
        poll(&watch.shared.queue),
        0,
        "the dropped registration took its event with it"
    );
}

/// A registration dropped while its pid is alive is deleted: the exit that follows is no event.
#[skuld::test]
fn a_registration_dropped_before_its_pid_exits_queues_nothing_when_it_does() {
    let watch = watch();
    let mut child = Held::new(0);
    let registration = registered(&watch, child.pid());
    drop(registration);

    child.finish();

    assert_eq!(poll(&watch.shared.queue), 0);
}

/// A registration's drop removes only its own pid's events: another registration's exit, read while
/// draining, is kept for it, and its wait still reports that exit.
#[skuld::test]
fn dropping_a_registration_keeps_a_siblings_exit_for_it() {
    let watch = watch();
    let (mut first, mut second) = (Held::new(3), Held::new(5));
    let dropped = registered(&watch, first.pid());
    let sibling = registered(&watch, second.pid());
    first.finish();
    second.finish();

    drop(dropped);

    assert_eq!(poll(&watch.shared.queue), 0, "the drain read everything the queue held");
    assert_eq!(sibling.wait(failure_bound()).expect("wait"), Some(5 << 8));
}

/// A wait for one pid takes that pid's exit and keeps another's, which was read first, for its own wait.
#[skuld::test]
fn a_wait_keeps_the_exits_it_reads_for_other_pids() {
    let watch = watch();
    let (mut first, mut second) = (Held::new(3), Held::new(5));
    let one = registered(&watch, first.pid());
    let other = registered(&watch, second.pid());
    first.finish();
    second.finish();

    assert_eq!(other.wait(failure_bound()).expect("wait"), Some(5 << 8));
    assert_eq!(one.wait(failure_bound()).expect("wait"), Some(3 << 8));
}

/// What deleting a registration answers, measured: where its event fired and was not read, it is still
/// registered (and its event goes with it); where the event was read, a registration of an exit is
/// gone (`ENOENT`). `Drop` does not depend on either.
#[skuld::test]
fn deleting_a_registration_whose_exit_fired_answers_as_measured() {
    let watch = watch();

    let mut unread = Held::new(0);
    let registration = registered(&watch, unread.pid());
    unread.finish();
    let answer = registration.delete();
    assert_eq!(answer, Ok(()), "a fired knote whose event is unread");
    assert_eq!(pending(&registration), 0, "its event goes with it");

    let mut read = Held::new(0);
    let registration = registered(&watch, read.pid());
    read.finish();
    assert_eq!(pending(&registration), 1, "the event, read");
    let answer = registration.delete();
    assert_eq!(answer, Err(Errno::ENOENT), "a fired knote whose event was read");
}

// A launch's registration =============================================================================================

/// An interruptible watch, and the window an interrupt is recorded in. The stand-in takes the signals that
/// are re-raised when the window closes, and the turn that keeps another test's dispositions out of this.
fn interruptible() -> (StandIn, Deferred, Watch) {
    let stand_in = StandIn::install();
    let window = Deferred::begin().expect("begin the window");
    let watch = Watch::new_interruptible().expect("a kqueue");
    (stand_in, window, watch)
}

/// `pid` registered as a launch, which must be alive.
fn launched(watch: &Watch, pid: u32) -> Registered {
    match watch.add_launch(pid).expect("add_launch") {
        Added::Watching(registered) => registered,
        Added::Gone => panic!("process {pid} is gone, and it was held alive"),
    }
}

/// An interrupt sent to this process, not to this thread: a kqueue has its event for the first only, which
/// is what a terminal's or a service manager's interrupt is. (`raise` is thread-directed.)
fn send_interrupt() {
    // SAFETY: `kill` takes plain integers, and every signal sent here has a handler.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0, "send SIGINT");
}

fn raise_interrupt() {
    // SAFETY: `raise` takes a valid signal number, and the window's handler is in place.
    assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
}

/// The first exec of a launch's pid is reported, and its exit is kept for the wait that follows: exec and
/// exit both happened before anything asked.
#[skuld::test]
fn a_launch_reports_its_first_exec_and_keeps_its_exit() {
    let (_stand_in, window, watch) = interruptible();
    let mut child = Held::execing(3);
    let registered = launched(&watch, child.pid());
    child.finish();

    assert!(matches!(
        registered.first_exec(&window).expect("first_exec"),
        Woke::Exec
    ));

    assert!(
        matches!(registered.exit(&window).expect("exit"), Woke::Exited(Some(status)) if status == 3 << 8),
        "the exit was kept"
    );
}

/// A launch that exits without an exec is reported as its exit.
#[skuld::test]
fn a_launch_that_exits_without_an_exec_is_its_exit() {
    let (_stand_in, window, watch) = interruptible();
    let mut child = Held::new(2);
    let registered = launched(&watch, child.pid());
    child.finish();

    assert!(
        matches!(registered.first_exec(&window).expect("first_exec"), Woke::Exited(Some(status)) if status == 2 << 8)
    );
}

/// A registration made for a launch is still a registration of one pid, deleted on drop: the exec and the
/// exit that follow queue nothing.
#[skuld::test]
fn a_launch_registration_dropped_early_queues_nothing() {
    let (_stand_in, _window, watch) = interruptible();
    let mut child = Held::execing(0);
    drop(launched(&watch, child.pid()));

    child.finish();

    assert_eq!(poll(&watch.shared.queue), 0);
}

/// An interrupt recorded before the wait is seen before it blocks, whatever the pid does: neither wait
/// returns the pid's state, and the pid is still alive.
#[skuld::test]
fn an_interrupt_recorded_before_a_wait_ends_it_before_it_blocks() {
    let (_stand_in, window, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());
    raise_interrupt();

    assert!(matches!(
        registered.first_exec(&window).expect("first_exec"),
        Woke::Interrupted
    ));
    assert!(matches!(registered.exit(&window).expect("exit"), Woke::Interrupted));
}

/// An interrupt that arrives while the wait blocks wakes it. A second process sends SIGINT to this one
/// once its stdin closes, which a thread does as the wait begins: whether the signal lands before the wait
/// blocks (recorded) or after (the kqueue's event), the wait ends.
#[skuld::test]
fn an_interrupt_during_a_wait_wakes_it() {
    let (_stand_in, window, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());
    let mut sender = Held::running("read _; kill -INT $PPID");
    let sending = std::thread::spawn(move || sender.finish());

    let woke = registered.exit(&window).expect("exit");

    assert!(matches!(woke, Woke::Interrupted));
    sending.join().expect("the sender's thread");
}

/// Sends an interrupt and waits until the stand-in handler has run for it: the handler that is there when
/// it is delivered, which is not the window's, so the window records nothing of it.
fn interrupt_before_a_window(stand_in: &StandIn) {
    send_interrupt();
    while stand_in.seen() == 0 {
        std::thread::yield_now();
    }
}

/// A signal whose event a read took out of the queue before the window's handler recorded it is not lost:
/// the watch has it. The signal arrives before a window is open, so the queue has its event and the record
/// has nothing; a poll then reads the event.
#[skuld::test]
fn a_signal_event_read_by_a_poll_is_still_an_interrupt() {
    let stand_in = StandIn::install();
    let watch = Watch::new_interruptible().expect("a kqueue");
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());
    interrupt_before_a_window(&stand_in);
    let window = Deferred::begin().expect("begin the window");
    assert!(!window.interrupted(), "the handler recorded nothing");

    assert!(!registered.has_exited().expect("poll"), "the poll reads the event");

    assert!(watch.interrupted(&window), "the watch has it");
    assert!(matches!(registered.exit(&window).expect("exit"), Woke::Interrupted));
}

/// The same for an event that the drop of another registration drained.
#[skuld::test]
fn a_signal_event_drained_by_a_dropped_registration_is_still_an_interrupt() {
    let stand_in = StandIn::install();
    let watch = Watch::new_interruptible().expect("a kqueue");
    let (child, other) = (Held::new(0), Held::new(0));
    let dropped = launched(&watch, other.pid());
    let registered = launched(&watch, child.pid());
    interrupt_before_a_window(&stand_in);
    let window = Deferred::begin().expect("begin the window");
    assert!(!window.interrupted(), "the handler recorded nothing");

    drop(dropped);

    assert!(watch.interrupted(&window), "the watch has it");
    assert!(matches!(registered.exit(&window).expect("exit"), Woke::Interrupted));
}

/// A watch that has seen no interrupt says none.
#[skuld::test]
fn a_watch_with_no_signal_is_not_interrupted() {
    let (_stand_in, window, watch) = interruptible();
    let child = Held::new(0);
    let registered = launched(&watch, child.pid());

    assert!(!registered.has_exited().expect("poll"));

    assert!(!watch.interrupted(&window));
}

/// Whether the pid has exited is read without blocking and without taking the exit: a wait after it still
/// reports it, and so does a second poll.
#[skuld::test]
fn has_exited_reports_an_exit_without_taking_it() {
    let watch = watch();
    let mut child = Held::new(3);
    let registered = registered(&watch, child.pid());

    assert!(!registered.has_exited().expect("poll"));
    child.finish();
    assert!(registered.has_exited().expect("poll"), "the exit");
    assert!(registered.has_exited().expect("poll again"), "not taken");

    assert_eq!(registered.wait(failure_bound()).expect("wait"), Some(3 << 8));
}

/// A poll for the exit keeps the exec it reads for the wait for the exec.
#[skuld::test]
fn a_poll_keeps_the_exec_for_the_wait_for_it() {
    let (_stand_in, window, watch) = interruptible();
    let mut child = Held::execing(3);
    let registered = launched(&watch, child.pid());
    child.finish();

    assert!(registered.has_exited().expect("poll"));

    assert!(matches!(
        registered.first_exec(&window).expect("first_exec"),
        Woke::Exec
    ));
}
