//! `start`, `stop` and `kill` of a task over the table-driven fake of Task Scheduler, with the machine's
//! processes a table the test states.

use std::io;
use std::time::Duration;

use windows::Win32::Foundation::SCHED_E_TASK_NOT_RUNNING;

use super::super::support::{Machine, PATH, id, instance, instance_id, record, records, scm};
use super::*;
use crate::backend::scm::manager::task::api::Started;
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::runs::RunOutcome;

fn instances_call() -> Call {
    Call::Instances(PATH.to_owned())
}

fn stop_call(n: u8) -> Call {
    Call::Stop(PATH.to_owned(), instance_id(n))
}

/// The listing of the one run `1` with its process `pid`.
fn one(pid: u32) -> Result<Vec<Instance>> {
    Ok(vec![instance(1, pid)])
}

/// The listing of two runs, `1` and `2`, with the pids 100 and 200: what a password-logon user's overlap
/// (L2) or the user's own concurrent start makes.
fn two() -> Result<Vec<Instance>> {
    Ok(vec![instance(1, 100), instance(2, 200)])
}

fn stopped(fake: &Fake, machine: &mut Machine, runs: &Runs, budget: Budget) -> Result<()> {
    scm(fake).task_stop_with("stop", &id(), budget, runs, machine)
}

// Stop ================================================================================================================

#[skuld::test]
fn stop_of_an_idle_task_is_ok() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new()));
    let mut machine = Machine::new(&fake, &[]);

    stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap();

    assert_eq!(fake.calls(), [instances_call()]);
    fake.finish();
}

/// The run is held and the listing read again before anything is sent, so what is waited for is the
/// run's process and no stranger's; the process is waited for after the `Stop`, and its record is read
/// after the wait.
#[skuld::test]
fn stop_sends_the_stop_then_waits_for_the_process_and_reports_an_incomplete_record() {
    let (dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim.write(&record(1, RunOutcome::IncompleteStop)).unwrap());

    let failed = stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap_err();

    let report = failed.report().to_string();
    assert!(report.contains("could not end the program's process tree"), "{report}");
    assert_eq!(
        fake.calls(),
        [instances_call(), instances_call(), instances_call(), stop_call(1)]
    );
    assert_eq!(machine.held, [4242], "the process is held before the stop");
    assert_eq!(machine.waited, [(4242, 4)], "and waited for after it");
    assert!(machine.terminated.is_empty(), "a stop terminates nothing");
    fake.finish();
}

#[skuld::test]
fn stop_of_a_run_whose_record_says_stopped_is_a_stop() {
    let (dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim.write(&record(1, RunOutcome::Stopped)).unwrap());

    stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap();

    fake.finish();
}

/// A record that cannot be read does not say the stop was complete.
#[skuld::test]
fn a_record_that_cannot_be_read_fails_the_stop() {
    let (_dir, path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let garbage = path.join(format!("{}.json", instance_id(1).hex()));
    machine.on_exit = Box::new(move |_| std::fs::write(&garbage, b"not a record").unwrap());

    let failed = stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap_err();

    assert!(failed.report().to_string().contains("read the record"), "{failed}");
}

/// A budget that does not wait sends the `Stop` and returns: nothing is held and nothing is waited for.
#[skuld::test]
fn stop_immediate_sends_and_returns() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242)).on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);

    stopped(&fake, &mut machine, &runs, Budget::Immediate).unwrap();
    assert_eq!(fake.calls(), [instances_call(), stop_call(1)]);
    assert!(machine.held.is_empty() && machine.waited.is_empty());

    // A zero bound is no wait either.
    let fake = Fake::new();
    fake.on_instances(one(4242)).on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    stopped(&fake, &mut machine, &runs, Budget::Bounded(Duration::ZERO)).unwrap();
    assert!(machine.held.is_empty() && machine.waited.is_empty());
    fake.finish();
}

#[skuld::test]
fn an_expired_stop_timeout_is_wait_timeout() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    machine.lingering = vec![4242];

    let failed = stopped(&fake, &mut machine, &runs, Budget::Bounded(Duration::from_millis(50))).unwrap_err();

    assert!(
        matches!(failed, Error::WaitTimeout { awaited: "stopped", .. }),
        "{failed:?}"
    );
    assert!(
        fake.calls().contains(&stop_call(1)),
        "the stop was sent: it is not cancelled"
    );
}

/// L3: a run that ends between the check and the call fails the `Stop` with `SCHED_E_TASK_NOT_RUNNING`,
/// which is a stopped run, waited for like any.
#[skuld::test]
fn stop_of_an_instance_that_ends_between_the_check_and_the_call_is_ok() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_stop(Err(Error::os(
            "stop an instance",
            windows::core::Error::from(SCHED_E_TASK_NOT_RUNNING),
        )));
    let mut machine = Machine::new(&fake, &[4242]);

    stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap();

    assert_eq!(machine.waited.len(), 1);
    fake.finish();
}

#[skuld::test]
fn any_other_failure_of_a_stop_is_the_stops_failure() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_stop(Err(Error::os("stop an instance", io::Error::other("denied"))));
    let mut machine = Machine::new(&fake, &[4242]);

    assert!(stopped(&fake, &mut machine, &runs, Budget::Unbounded).is_err());
    assert!(machine.waited.is_empty());
}

/// Every run that is alive ends, and every process is held before the first `Stop` is sent, so a
/// process that cannot be held fails the stop with nothing sent.
#[skuld::test]
fn stop_and_kill_end_every_live_instance() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_stop(Ok(()))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[100, 200]);

    stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap();

    assert_eq!(machine.held, [100, 200]);
    let stops: Vec<Call> = fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Stop(..)))
        .collect();
    assert_eq!(stops, [stop_call(1), stop_call(2)]);
    assert_eq!(
        machine.waited.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
        [100, 200]
    );
    assert!(
        machine.waited.iter().all(|(_, calls)| *calls >= 7),
        "both are stopped before either is waited for: {:?}",
        machine.waited
    );
    fake.finish();

    // And a kill: both are terminated, each through its own held process.
    let fake = Fake::new();
    fake.on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_instances(two());
    let mut machine = Machine::new(&fake, &[100, 200]);
    scm(&fake).task_kill_with(&id(), &mut machine).unwrap();
    assert_eq!(machine.held, [100, 200]);
    assert_eq!(
        machine.terminated.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
        [100, 200]
    );
    assert_eq!(
        machine.waited.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
        [100, 200]
    );
    fake.finish();
}

#[skuld::test]
fn an_instance_that_cannot_be_opened_fails_the_stop_before_any_stop_is_sent() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    // The probe, the first run's hold and its second read, the second run's hold, and the read that
    // tells a refusal from a stale pid.
    fake.on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_instances(two())
        .on_instances(two());
    let mut machine = Machine::new(&fake, &[100, 200]);
    machine.denied = vec![200];

    let failed = stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap_err();

    assert!(failed.report().to_string().contains("denied access"), "{failed}");
    assert!(!fake.calls().iter().any(|call| matches!(call, Call::Stop(..))));
    assert!(machine.waited.is_empty());
}

// No listing wait -----------------------------------------------------------------------------------------------------

/// L2: an ended run stays listed for a few milliseconds, and nothing waits for it to drop. Its process
/// is gone, so it is no run: `stop` finds none and `kill` finds none, and both return.
#[skuld::test]
fn no_verb_waits_for_the_listing_to_drop() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242)).on_instances(one(4242));
    let mut machine = Machine::new(&fake, &[]);

    stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap();
    let killed = scm(&fake).task_kill_with(&id(), &mut machine).unwrap_err();

    assert!(matches!(killed, Error::NotRunning { .. }), "{killed:?}");
    assert_eq!(fake.calls(), [instances_call(), instances_call()]);
    assert!(machine.held.is_empty() && machine.waited.is_empty());
    fake.finish();
}

/// A listing that still names a process that is gone when it is opened is no process: the hold ends
/// there, and neither `stop` nor `kill` reads the listing again to wait for it to change.
#[skuld::test]
fn a_listed_instance_with_a_dead_process_is_no_process_and_does_not_spin() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    // The probe says alive; the open then finds no process. Each verb reads the listing for the probe
    // and once to hold, and no more.
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242));
    let mut machine = Machine::new(&fake, &[4242]);
    machine.holdable.clear();

    stopped(&fake, &mut machine, &runs, Budget::Unbounded).unwrap();
    scm(&fake).task_kill_with(&id(), &mut machine).unwrap();

    assert!(!fake.calls().iter().any(|call| matches!(call, Call::Stop(..))));
    assert!(machine.waited.is_empty() && machine.terminated.is_empty());
    fake.finish();
}

// Kill ================================================================================================================

#[skuld::test]
fn kill_terminates_through_the_held_handle_after_the_second_read_confirms_it() {
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(one(4242));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake).task_kill_with(&id(), &mut machine).unwrap();

    assert_eq!(machine.held, [4242]);
    assert_eq!(
        machine.terminated,
        [(4242, 3)],
        "after the live read, the hold's read and its second read"
    );
    assert_eq!(machine.waited, [(4242, 3)]);
    assert!(
        !fake.calls().iter().any(|call| matches!(call, Call::Stop(..))),
        "no Stop is sent"
    );
    fake.finish();
}

/// A pid the listing no longer names after the open may be a stranger's: nothing is terminated.
#[skuld::test]
fn kill_of_a_run_that_left_the_listing_after_it_was_held_terminates_nothing() {
    let fake = Fake::new();
    fake.on_instances(one(4242))
        .on_instances(one(4242))
        .on_instances(Ok(Vec::new()));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake).task_kill_with(&id(), &mut machine).unwrap();

    assert_eq!(machine.held, [4242]);
    assert!(machine.terminated.is_empty() && machine.waited.is_empty());
    fake.finish();
}

#[skuld::test]
fn kill_of_an_idle_task_is_not_running() {
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new()));
    let mut machine = Machine::new(&fake, &[]);

    let failed = scm(&fake).task_kill_with(&id(), &mut machine).unwrap_err();

    assert!(matches!(failed, Error::NotRunning { .. }), "{failed:?}");
}

// Start ===============================================================================================================

/// `start` is `Run` and returns: never `RunEx` with `TASK_RUN_IGNORE_CONSTRAINTS` (the boundary has no
/// such call), and nothing is read first or after.
#[skuld::test]
fn start_calls_run_and_never_run_ex_with_ignore_constraints() {
    let fake = Fake::new();
    fake.on_run(Ok(Started {
        id: instance_id(1),
        pid: Some(4242),
    }));

    scm(&fake).task_start(&id()).unwrap();

    assert_eq!(fake.calls(), [Call::Run(PATH.to_owned())]);
    fake.finish();
}

/// A task that was ignored (a live run, `IgnoreNew`) answers a `Run` with a GUID and no pid: a start
/// that was accepted.
#[skuld::test]
fn a_start_that_started_nothing_is_still_a_start_that_was_accepted() {
    let fake = Fake::new();
    fake.on_run(Ok(Started {
        id: instance_id(1),
        pid: None,
    }));

    scm(&fake).task_start(&id()).unwrap();
}

/// `restart` is a stop and then a start. The run that was stopped can still be listed, and its process
/// is gone: the stop finds nothing to wait for, and the start is made all the same.
#[skuld::test]
fn restart_stops_then_starts_and_starts_when_an_ended_instance_is_still_listed() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_instances(one(4242)).on_run(Ok(Started {
        id: instance_id(2),
        pid: Some(5252),
    }));
    let mut machine = Machine::new(&fake, &[]);
    let manager = scm(&fake);

    manager
        .task_stop_with("stop", &id(), Budget::Unbounded, &runs, &mut machine)
        .unwrap();
    manager.task_start(&id()).unwrap();

    assert_eq!(fake.calls(), [instances_call(), Call::Run(PATH.to_owned())]);
    fake.finish();
}
