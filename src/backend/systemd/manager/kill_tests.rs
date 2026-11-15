//! [`super::UnitKill`] against a [`Fake`] table of `systemctl show` answers, with real child processes
//! where a signal has to land.

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, Command, Stdio};

use super::*;
use crate::backend::Backend;
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::kill::{KillOutcome, kill_failed, kill_service};
use crate::backend::scm::test_support::{spent, unbounded};
use crate::backend::systemd::manager::systemctl::systemctl_tests::{booted, gate_then, on_booted};
use crate::backend::systemd::manager::task_tests::{fragment, service_spec};
use crate::error::Error;
use crate::manager::{Budget, Step};

/// `systemctl show` answering `MainPID=pid` for a loaded unit.
fn show(pid: u32) -> Entry {
    reply(&["show"], exited(0, &format!("MainPID={pid}\nLoadState=loaded\n"), ""))
}

/// `systemctl show` of a unit it does not have loaded.
fn not_loaded() -> Entry {
    reply(&["show"], exited(0, "MainPID=0\nLoadState=not-found\n", ""))
}

/// The target of `x.service`, over `systemctl`.
fn target(systemctl: &Systemctl) -> UnitKill<'_> {
    UnitKill::new(systemctl, "x.service".to_string())
}

/// A child that runs until it is signalled, killed and reaped on drop so a failing assert leaves
/// nothing running.
struct Sleeper(Child);

impl Sleeper {
    fn spawn() -> Self {
        let child = Command::new("sleep")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        Sleeper(child)
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// The pid ---------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn the_pid_is_the_main_pid_systemd_shows_for_the_unit() {
    let fake = Fake::new([show(4242)]);
    let systemctl = on_booted(fake.clone());

    assert_eq!(target(&systemctl).service_pid().unwrap(), Some(4242));
    assert_eq!(
        fake.calls(),
        [["show", "--property", "MainPID,LoadState", "--", "x.service"]]
    );
}

#[skuld::test]
fn a_unit_with_no_main_process_has_no_pid() {
    let systemctl = on_booted(Fake::new([show(0)]));

    assert_eq!(target(&systemctl).service_pid().unwrap(), None);
}

/// What `systemctl kill` called "Unit x.service not loaded.": an inactive unit nothing holds.
#[skuld::test]
fn a_unit_systemd_has_not_loaded_has_no_pid() {
    let systemctl = on_booted(Fake::new([not_loaded()]));

    assert_eq!(target(&systemctl).service_pid().unwrap(), None);
}

#[skuld::test]
fn a_show_that_fails_is_an_error_and_never_no_process() {
    let systemctl = on_booted(Fake::new([reply(
        &["show"],
        exited(1, "", "Failed to connect to bus\n"),
    )]));

    let e = target(&systemctl).service_pid().unwrap_err();

    let e = kill_failed("x", e);
    assert!(matches!(e, Error::Command { .. }), "{e:?}");
}

// The wait for the record -----------------------------------------------------------------------------------------------

/// systemd clears `MainPID` when it reaps the process, after the process has exited: a read straight
/// after the exit can still show the pid.
#[skuld::test]
fn the_record_is_waited_for_until_the_main_pid_is_not_the_killed_one() {
    let fake = Fake::new([show(100), show(100), show(0)]);
    let systemctl = on_booted(fake.clone());

    let waited = target(&systemctl).await_recorded(100, unbounded()).unwrap();

    assert_eq!(waited, Waited::Confirmed);
    assert_eq!(fake.calls().len(), 3);
}

/// An auto-restart that has already forked again: the killed pid is not the main one.
#[skuld::test]
fn a_new_main_pid_is_the_record() {
    let systemctl = on_booted(Fake::new([show(101)]));

    assert_eq!(
        target(&systemctl).await_recorded(100, unbounded()).unwrap(),
        Waited::Confirmed
    );
}

/// A unit that systemd has let go of, with the process gone, reports no pid.
#[skuld::test]
fn an_unloaded_unit_is_the_record() {
    let systemctl = on_booted(Fake::new([show(100), not_loaded()]));

    assert_eq!(
        target(&systemctl).await_recorded(100, unbounded()).unwrap(),
        Waited::Confirmed
    );
}

#[skuld::test]
fn a_deadline_that_has_passed_is_read_once_and_expires() {
    let fake = Fake::new([show(100)]);
    let systemctl = on_booted(fake.clone());

    assert_eq!(
        target(&systemctl).await_recorded(100, spent()).unwrap(),
        Waited::Expired
    );
    assert_eq!(fake.calls().len(), 1);
}

#[skuld::test]
fn a_read_that_fails_in_the_wait_is_an_error() {
    let systemctl = on_booted(Fake::new([
        show(100),
        reply(&["show"], exited(1, "", "Failed to connect to bus\n")),
    ]));

    assert!(target(&systemctl).await_recorded(100, unbounded()).is_err());
}

// The kill --------------------------------------------------------------------------------------------------------------

/// The process is held by pidfd, confirmed by a second read, signalled, waited for, and then the
/// record is waited for: the whole machine, over a real process.
#[skuld::test]
fn kill_signals_the_main_process_and_returns_once_systemd_has_recorded_it() {
    let mut sleeper = Sleeper::spawn();
    let pid = sleeper.0.id();
    // The read, the confirmation, a read that still shows the process, and the record.
    let fake = Fake::new([show(pid), show(pid), show(pid), show(0)]);
    let systemctl = on_booted(fake.clone());

    assert_eq!(kill_service(&mut target(&systemctl)).unwrap(), KillOutcome::Ended);

    assert_eq!(sleeper.0.wait().unwrap().signal(), Some(9));
    assert_eq!(fake.calls().len(), 4);
}

#[skuld::test]
fn kill_of_a_unit_with_no_main_process_is_not_running() {
    for answer in [show(0), not_loaded()] {
        let fake = Fake::new([answer]);
        let systemctl = on_booted(fake.clone());

        assert_eq!(kill_service(&mut target(&systemctl)).unwrap(), KillOutcome::NotRunning);
        assert_eq!(fake.calls().len(), 1, "nothing but the read");
    }
}

/// The manager's version gate, which every request asks, is asked before the unit is read.
#[skuld::test]
fn the_backend_kill_asks_the_gate_and_reports_a_unit_with_no_process_as_not_running() {
    let fake = gate_then([show(0)]);
    let mgr = Systemd::with(Systemctl::new(Box::new(fake.clone()), booted));
    let mut held = Backend::reserve(&mgr, &[Step::Kill], Budget::Unbounded).expect("reserve");

    let e = Backend::kill(&mgr, fragment(&service_spec("x")), &mut held).expect_err("no process");

    assert!(matches!(&e, Error::NotRunning { id } if id == "x"), "{e:?}");
    let calls = fake.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], ["--version"]);
    assert_eq!(calls[2][0], "show");
    assert_eq!(calls[2].last().map(String::as_str), Some("x.service"));
}
