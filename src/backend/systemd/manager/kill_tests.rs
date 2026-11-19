//! [`super::UnitKill`] against a [`Fake`] table of `systemctl show` answers, with real child processes
//! where a signal has to land.

use std::os::unix::process::ExitStatusExt as _;

use super::*;
use crate::backend::Backend;
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::kill::process::process_tests::Sleeper;
use crate::backend::kill::{KillOutcome, KillTarget as _, kill_failed, kill_service};
use crate::backend::systemd::manager::Systemd;
use crate::backend::systemd::manager::systemctl::systemctl_tests::{booted, gate_then, on_booted};
use crate::backend::systemd::manager::task::task_tests::{fragment, service_spec};
use crate::error::Error;
use crate::manager::budget::Budget as Bound;
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
fn target(systemctl: &Systemctl) -> Signalling<UnitKill<'_>> {
    UnitKill::new(systemctl, "x.service".to_string())
}

/// A child that has exited and not been reaped, held as `kill` holds it: what systemd still reports
/// as the unit's `MainPID` until it reaps it.
fn unreaped() -> (Sleeper, Process) {
    let run = Sleeper::spawn();
    let held = process::open(run.pid())
        .expect("open the child")
        .expect("it is running");
    process::terminate(&held).expect("signal the child");
    let waited = process::await_exit(&held, Bound::Unbounded.start()).expect("wait for it to exit");
    assert_eq!(waited, Waited::Confirmed);
    (run, held)
}

// The pid -------------------------------------------------------------------------------------------------------------

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

/// A unit systemd has not loaded is one goetia cannot see, not one with no process: the error `status`
/// gives, never a state.
#[skuld::test]
fn a_unit_systemd_has_not_loaded_is_an_error_and_never_no_process() {
    let systemctl = on_booted(Fake::new([not_loaded()]));

    let e = kill_failed("x", target(&systemctl).service_pid().unwrap_err());

    assert!(
        matches!(e.cause(), Error::UnitNotLoaded { unit } if unit == "x.service"),
        "{e:?}"
    );
}

#[skuld::test]
fn a_show_that_fails_is_an_error_and_never_no_process() {
    let systemctl = on_booted(Fake::new([reply(
        &["show"],
        exited(1, "", "Failed to connect to bus\n"),
    )]));

    let e = target(&systemctl).service_pid().unwrap_err();

    let e = kill_failed("x", e);
    assert!(matches!(e.cause(), Error::Command { .. }), "{e:?}");
}

// The wait for the record ---------------------------------------------------------------------------------------------

/// systemd clears `MainPID` when it reaps the process, after the process has exited: a read straight
/// after the exit can still show the pid, and the process, not yet reaped, is still the held one.
#[skuld::test]
fn the_record_is_waited_for_until_the_main_pid_is_not_the_killed_one() {
    let (run, held) = unreaped();
    let pid = run.pid();
    let fake = Fake::new([show(pid), show(pid), show(0)]);
    let systemctl = on_booted(fake.clone());

    let waited = target(&systemctl)
        .await_recorded(pid, Some(&held), Bound::Unbounded.start())
        .unwrap();

    assert_eq!(waited, Waited::Confirmed);
    assert_eq!(fake.calls().len(), 3);
}

/// A restart that forked again before the killed process was recorded: the killed pid is not the main
/// one, whatever it names.
#[skuld::test]
fn a_new_main_pid_is_the_record() {
    let systemctl = on_booted(Fake::new([show(101)]));

    assert_eq!(
        target(&systemctl)
            .await_recorded(100, None, Bound::Unbounded.start())
            .unwrap(),
        Waited::Confirmed
    );
}

/// systemd reaped the process and restarted the unit, and the new process was given the same pid: the
/// number shown is the same, the process is not, and the end of the killed one is recorded.
#[skuld::test]
fn a_new_process_with_the_killed_pid_is_the_record() {
    let (_run, held) = unreaped();
    let newcomer = Sleeper::spawn();
    // What the number names now is `newcomer`, whose pid stands in for the one the new process was given.
    let fake = Fake::new([show(newcomer.pid())]);
    let systemctl = on_booted(fake.clone());

    let waited = target(&systemctl)
        .await_recorded(newcomer.pid(), Some(&held), Bound::Unbounded.start())
        .unwrap();

    assert_eq!(waited, Waited::Confirmed);
    assert_eq!(
        fake.calls().len(),
        1,
        "the first read shows the pid, and the identity settles it"
    );
}

/// A process reaped but still shown has not been recorded yet: the wait reads again.
#[skuld::test]
fn a_reaped_process_the_manager_still_shows_is_waited_for() {
    let (mut run, held) = unreaped();
    let pid = run.pid();
    run.0.wait().expect("reap the child");
    let fake = Fake::new([show(pid), show(0)]);
    let systemctl = on_booted(fake.clone());

    let waited = target(&systemctl)
        .await_recorded(pid, Some(&held), Bound::Unbounded.start())
        .unwrap();

    assert_eq!(waited, Waited::Confirmed);
    assert_eq!(fake.calls().len(), 2);
}

/// A unit systemd reports as not loaded in the wait is no record: it is the error `status` gives.
#[skuld::test]
fn an_unloaded_unit_in_the_wait_is_an_error_and_not_the_record() {
    let (run, held) = unreaped();
    let pid = run.pid();
    let systemctl = on_booted(Fake::new([show(pid), not_loaded()]));

    let e = kill_failed(
        "x",
        target(&systemctl)
            .await_recorded(pid, Some(&held), Bound::Unbounded.start())
            .unwrap_err(),
    );

    assert!(matches!(e.cause(), Error::UnitNotLoaded { .. }), "{e:?}");
}

#[skuld::test]
fn a_deadline_that_has_passed_is_read_once_and_expires() {
    let (run, held) = unreaped();
    let pid = run.pid();
    let fake = Fake::new([show(pid)]);
    let systemctl = on_booted(fake.clone());

    assert_eq!(
        target(&systemctl)
            .await_recorded(pid, Some(&held), Bound::Immediate.start())
            .unwrap(),
        Waited::Expired
    );
    assert_eq!(fake.calls().len(), 1);
}

#[skuld::test]
fn a_read_that_fails_in_the_wait_is_an_error() {
    let (run, held) = unreaped();
    let pid = run.pid();
    let systemctl = on_booted(Fake::new([
        show(pid),
        reply(&["show"], exited(1, "", "Failed to connect to bus\n")),
    ]));

    assert!(
        target(&systemctl)
            .await_recorded(pid, Some(&held), Bound::Unbounded.start())
            .is_err()
    );
}

// The kill ------------------------------------------------------------------------------------------------------------

/// The process is held by pidfd, confirmed by a second read, signalled, waited for, and then the
/// record is waited for: the whole machine, over a real process.
#[skuld::test]
fn kill_signals_the_main_process_and_returns_once_systemd_has_recorded_it() {
    let mut sleeper = Sleeper::spawn();
    let pid = sleeper.pid();
    // The read, the confirmation, a read that still shows the process, and the record.
    let fake = Fake::new([show(pid), show(pid), show(pid), show(0)]);
    let systemctl = on_booted(fake.clone());

    assert_eq!(kill_service(&mut target(&systemctl), "x").unwrap(), KillOutcome::Ended);

    assert_eq!(sleeper.0.wait().unwrap().signal(), Some(9));
    assert_eq!(fake.calls().len(), 4);
}

#[skuld::test]
fn kill_of_a_unit_with_no_main_process_is_not_running() {
    let fake = Fake::new([show(0)]);
    let systemctl = on_booted(fake.clone());

    assert_eq!(
        kill_service(&mut target(&systemctl), "x").unwrap(),
        KillOutcome::NotRunning
    );
    assert_eq!(fake.calls().len(), 1, "nothing but the read");
}

/// What `status` says of a unit systemd has not loaded, `kill` says: it is not "not running".
#[skuld::test]
fn kill_of_a_unit_systemd_has_not_loaded_is_the_error_status_gives() {
    let fake = gate_then([not_loaded()]);
    let mgr = Systemd::with(Systemctl::new(Box::new(fake.clone()), booted));
    let mut held = Backend::reserve(&mgr, &[Step::Kill], Budget::Unbounded).expect("reserve");

    let e = Backend::kill(&mgr, fragment(&service_spec("x")), &mut held).expect_err("not loaded");

    assert!(
        matches!(e.cause(), Error::UnitNotLoaded { unit } if unit == "x.service"),
        "{e:?}"
    );
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
