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

/// A child that exits while nobody is waiting still has its status reported: the event is held by the
/// queue from the moment of the registration, which is made while the child lives.
#[skuld::test]
fn watch_reports_a_childs_exit_status_after_it_ended_quickly() {
    let watch = watch();
    let mut child = Held::new(3);
    assert_eq!(watch.add(child.pid()).expect("add"), Added::Watching);

    assert_eq!(child.finish(), Some(3));

    assert_eq!(watch.wait(child.pid(), failure_bound()).expect("wait"), Some(3 << 8));
}

#[skuld::test]
fn one_kqueue_serves_two_pids_in_turn() {
    let watch = watch();
    for code in [3, 5] {
        let mut child = Held::new(code);
        assert_eq!(watch.add(child.pid()).expect("add"), Added::Watching);
        child.finish();
        assert_eq!(watch.wait(child.pid(), failure_bound()).expect("wait"), Some(code << 8));
    }
}

/// A wait for one pid takes that pid's event and passes over another's, which exited first.
#[skuld::test]
fn wait_takes_that_pids_event_and_passes_over_the_others() {
    let watch = watch();
    let (mut first, mut second) = (Held::new(3), Held::new(5));
    assert_eq!(watch.add(first.pid()).expect("add"), Added::Watching);
    assert_eq!(watch.add(second.pid()).expect("add"), Added::Watching);
    first.finish();
    second.finish();

    assert_eq!(watch.wait(second.pid(), failure_bound()).expect("wait"), Some(5 << 8));
}

/// A signalled process reports the signal in the status.
#[skuld::test]
fn a_killed_child_reports_its_signal() {
    let watch = watch();
    let mut child = Held::new(0);
    assert_eq!(watch.add(child.pid()).expect("add"), Added::Watching);
    child.child.kill().expect("kill the child");
    child.child.wait().expect("reap it");

    let status = watch
        .wait(child.pid(), failure_bound())
        .expect("wait")
        .expect("an exit");

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

    assert_eq!(watch().add(pid).expect("add"), Added::Gone);
}

#[skuld::test]
fn wait_with_an_expired_deadline_is_none() {
    let watch = watch();
    let child = Held::new(0);
    assert_eq!(watch.add(child.pid()).expect("add"), Added::Watching);

    assert_eq!(
        watch.wait(child.pid(), Some(Budget::Immediate.start())).expect("wait"),
        None
    );
}

/// An exit that is already queued is reported past a deadline that has expired, as a poll.
#[skuld::test]
fn an_exit_already_queued_is_reported_under_an_expired_deadline() {
    let watch = watch();
    let mut child = Held::new(2);
    assert_eq!(watch.add(child.pid()).expect("add"), Added::Watching);
    child.finish();

    assert_eq!(
        watch.wait(child.pid(), Some(Budget::Immediate.start())).expect("wait"),
        Some(2 << 8)
    );
}

#[skuld::test]
fn a_watch_that_cannot_be_made_is_an_error() {
    let _none = test_hook::queues(0);

    let e = Watch::new().err().expect("no kqueue may be made");

    assert!(e.to_string().contains("kqueue"), "{e}");
}
