//! The watch over real child processes, each held alive by an open stdin until the test lets it go.
//! Nothing here needs root, launchd or a file outside the process.

use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

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

    fn running(script: &str) -> Held {
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh");
        let stdin = child.stdin.take();
        Held { child, stdin }
    }

    pub(in crate::backend::launchd::manager) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Lets the child go and reaps it, so its exit has happened (and its event been queued) when this
    /// returns. Its exit code.
    pub(in crate::backend::launchd::manager) fn finish(&mut self) -> Option<i32> {
        drop(self.stdin.take());
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
    poll(&registered.queue)
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

    assert_eq!(poll(&watch.queue), 0, "the dropped registration took its event with it");
}

/// A registration dropped while its pid is alive is deleted: the exit that follows is no event.
#[skuld::test]
fn a_registration_dropped_before_its_pid_exits_queues_nothing_when_it_does() {
    let watch = watch();
    let mut child = Held::new(0);
    let registration = registered(&watch, child.pid());
    drop(registration);

    child.finish();

    assert_eq!(poll(&watch.queue), 0);
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
