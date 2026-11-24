//! Removing a task over the table-driven fake of Task Scheduler, with the run records in a temporary
//! directory and the machine's processes a table the tests state: which are alive, which can be held,
//! and what a held one does when it is waited for (a shim writing its record).

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::SCHED_E_TASK_NOT_RUNNING;

use super::super::support::{
    Denied, Machine, PATH, id, instance, instance_id, linked_records, pins_in, record, records, scm, shim_writes,
    snapshot_of,
};
use super::*;
use crate::backend::ReadOutcome;
use crate::backend::kill::Waited;
use crate::backend::scm::manager::task::api::Registration;
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::manager::task::live::Liveness;
use crate::backend::scm::manager::task::write::Prepared;
use crate::backend::scm::runs::RunOutcome;
use crate::backend::scm::sid::system;
use crate::backend::scm::test_support::{registered_task, task_host, task_spec, task_spec_as};
use crate::manager::budget::Deadline;
use crate::spec::User;

fn disable_call() -> Call {
    Call::SetEnabled(PATH.to_owned(), false)
}

// Uninstall ===========================================================================================================

/// The order of an uninstall, as the scheduler sees it: the task is disabled first, so that nothing
/// starts another run; then the runs are stopped and waited for; then the task is deleted.
#[skuld::test]
fn uninstall_of_a_running_task_stops_it_then_deletes_it_and_its_records() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim_writes(&shim, &record(1, RunOutcome::Stopped)));

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [
            disable_call(),
            Call::Instances(PATH.to_owned()),
            Call::Instances(PATH.to_owned()),
            Call::Instances(PATH.to_owned()),
            Call::Stop(PATH.to_owned(), instance_id(1)),
            Call::Delete(PATH.to_owned()),
        ],
        "disabled before the runs are read and stopped, and stopped before the delete"
    );
    assert_eq!(machine.held, [4242], "the run's process is held before it is stopped");
    assert_eq!(
        machine.waited,
        [(4242, 5)],
        "its exit is waited for after the stop and before the delete"
    );
    assert!(!records_dir.exists(), "the records go with the task");
    fake.finish();
}

/// The account can make the task's records directory a link. The uninstall goes on all the same: the run
/// is stopped, and the link is deleted with what it points to left as it was.
#[skuld::test]
fn uninstall_of_a_task_whose_records_directory_is_a_link_stops_the_run_and_deletes_the_link() {
    let (_dir, records_dir, runs, target) = linked_records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(fake.calls().contains(&Call::Stop(PATH.to_owned(), instance_id(1))));
    assert!(std::fs::symlink_metadata(&records_dir).is_err(), "the link is gone");
    assert!(
        target.path().join(format!("{}.json", instance_id(1).hex())).exists(),
        "what it pointed to is as it was"
    );
    fake.finish();
}

/// Disabling is not a registration, so it is made for every task, armed or not, and whatever its account.
#[skuld::test]
fn uninstall_of_an_idle_disarmed_task_disables_it_before_it_deletes_it() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [
            disable_call(),
            Call::Instances(PATH.to_owned()),
            Call::Delete(PATH.to_owned())
        ]
    );
    assert!(!records_dir.exists());
}

/// Task Scheduler's folder for goetia's tasks that cannot be removed once the last task is deleted is
/// not the task's: the task and its records are gone, so the uninstall is done, and it warns.
#[skuld::test]
fn a_folder_left_after_the_delete_does_not_fail_the_uninstall() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::TaskFolderKept {
            task: PATH.to_owned(),
            folder: r"\Goetia".to_owned(),
            source: Box::new(io::Error::other("refused")),
        }));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(!records_dir.exists(), "the records go with the task");
    fake.finish();
}

/// What the shim could not end is a daemon that may run: the uninstall fails, with the task registered
/// and disabled, so that re-running it is the retry and nothing starts it meanwhile.
#[skuld::test]
fn uninstall_of_an_incomplete_stop_leaves_the_task_disabled() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim_writes(&shim, &record(1, RunOutcome::IncompleteStop)));

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();

    let text = failed.to_string();
    assert!(text.contains("NOT deleted") && text.contains("disabled"), "{text}");
    assert!(
        text.contains("goetia daemon uninstall nightly"),
        "the retry is named: {text}"
    );
    assert_eq!(
        fake.calls(),
        [
            disable_call(),
            Call::Instances(PATH.to_owned()),
            Call::Instances(PATH.to_owned()),
            Call::Instances(PATH.to_owned()),
            Call::Stop(PATH.to_owned(), instance_id(1)),
        ],
        "no delete"
    );
    assert!(records_dir.exists());
    fake.finish();
}

/// A record that cannot be read does not say the stop was complete: the uninstall fails and keeps the
/// task, disabled, rather than delete it with the program possibly still running.
#[skuld::test]
fn a_record_that_cannot_be_read_fails_the_uninstall_and_keeps_the_task() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let garbage = records_dir.join(format!("{}.json", instance_id(1).hex()));
    machine.on_exit = Box::new(move |_| std::fs::write(&garbage, b"not a record").unwrap());

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();

    let text = failed.to_string();
    assert!(text.contains("NOT deleted") && text.contains("disabled"), "{text}");
    assert!(
        !fake.calls().iter().any(|call| matches!(call, Call::Delete(..))),
        "nothing is deleted"
    );
    assert!(records_dir.exists());
}

/// The account's password is never read: an armed task of a real user is disabled and removed as any other,
/// whether or not the password is known, changed or the account gone (measured, N1 and N2).
#[skuld::test]
fn uninstall_of_an_armed_real_user_task_needs_no_password() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [
            disable_call(),
            Call::Instances(PATH.to_owned()),
            Call::Delete(PATH.to_owned())
        ],
        "no registration"
    );
    assert!(!records_dir.exists());
}

/// A task whose blob will not decode is removed all the same: nothing here reads the blob.
#[skuld::test]
fn uninstall_of_a_task_whose_blob_will_not_decode_needs_nothing_of_it() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let registered = registered_task(&spec, true);
    let broken = TaskSnapshot::new(
        id(),
        Err(std::sync::Arc::new(Error::blob("cannot read it"))),
        crate::backend::scm::manager::task::api::Registered {
            xml: registered.xml,
            data: registered.data.replace("Spec: ", "Spec: !"),
            enabled: registered.enabled,
        },
    );
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&broken, &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(!records_dir.exists());
}

#[skuld::test]
fn every_live_instance_is_stopped() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let both = || Ok(vec![instance(1, 100), instance(2, 200)]);
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(both())
        .on_instances(both())
        .on_instances(both())
        .on_instances(both())
        .on_instances(both())
        .on_stop(Ok(()))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[100, 200]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    let stops: Vec<Call> = fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Stop(..)))
        .collect();
    assert_eq!(
        stops,
        [
            Call::Stop(PATH.to_owned(), instance_id(1)),
            Call::Stop(PATH.to_owned(), instance_id(2))
        ]
    );
    assert_eq!(
        machine.waited.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
        [100, 200]
    );
    fake.finish();
}

/// The listing is not proof of life: an entry whose process has exited is not stopped, and nothing waits
/// for it.
#[skuld::test]
fn a_listed_instance_whose_process_is_gone_is_not_stopped() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [
            disable_call(),
            Call::Instances(PATH.to_owned()),
            Call::Delete(PATH.to_owned())
        ]
    );
    assert!(machine.held.is_empty() && machine.waited.is_empty());
}

/// A pid is held, and then the listing is read again: an entry that is no longer listed ended between,
/// and what was opened may be a stranger that took the number, so it is neither stopped nor waited for.
#[skuld::test]
fn a_run_that_ended_after_it_was_held_is_not_stopped_or_waited_for() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(machine.held, [4242]);
    assert!(machine.waited.is_empty());
    assert!(!fake.calls().iter().any(|call| matches!(call, Call::Stop(..))));
}

/// A `Stop` that finds the run already ended is a stopped run (L3), which is waited for like any.
#[skuld::test]
fn a_stop_that_finds_the_run_ended_is_a_stop() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Err(Error::os(
            "stop an instance",
            windows::core::Error::from(SCHED_E_TASK_NOT_RUNNING),
        )))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(machine.waited.len(), 1);
}

#[skuld::test]
fn any_other_failure_of_a_stop_fails_the_uninstall_and_deletes_nothing() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Err(Error::os("stop an instance", io::Error::other("denied"))));
    let mut machine = Machine::new(&fake, &[4242]);

    let failed = scm(&fake).task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine);

    assert!(failed.is_err());
    assert!(machine.waited.is_empty());
    assert!(records_dir.exists());
    assert!(!fake.calls().iter().any(|call| matches!(call, Call::Delete(..))));
}

/// A listing that cannot be told for an unreadable process fails the uninstall before a stop is sent
/// (the probe is the error, never "alive").
#[skuld::test]
fn a_process_that_cannot_be_judged_fails_the_uninstall_before_a_stop() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_instances(Ok(vec![instance(1, 4242)]));
    struct Denied;
    impl Processes for Denied {
        type Held = ();
        fn alive(&mut self, _: u32) -> io::Result<Liveness> {
            Err(io::Error::from_raw_os_error(5))
        }
        fn open(&mut self, _: u32) -> io::Result<Option<()>> {
            unreachable!("a process that cannot be judged is not held")
        }
        fn force_end(&mut self, _: &()) -> io::Result<()> {
            unreachable!()
        }
        fn await_exit(&mut self, _: &(), _: Deadline) -> io::Result<Waited> {
            unreachable!()
        }
    }

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut Denied)
        .unwrap_err();

    let report = failed.report().to_string();
    assert!(report.contains("denied access"), "{report}");
    assert_eq!(fake.calls(), [disable_call(), Call::Instances(PATH.to_owned())]);
}

/// A task that goes between the read and the uninstall has nothing to disable or stop; the delete finds
/// none, and the records still go.
#[skuld::test]
fn a_task_that_went_meanwhile_is_uninstalled_all_the_same() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_instances(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_delete(Ok(false));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(!records_dir.exists());
}

/// A disable that fails for any other reason is the uninstall's failure, before anything is stopped or
/// deleted.
#[skuld::test]
fn a_failed_disable_fails_the_uninstall_before_anything_else() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Err(Error::os("set the enabled flag", io::Error::other("denied"))));
    let mut machine = Machine::new(&fake, &[]);

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();

    assert!(failed.to_string().contains("NOT deleted"), "{failed}");
    assert_eq!(fake.calls(), [disable_call()]);
    assert!(records_dir.exists());
}

#[skuld::test]
fn a_failed_delete_leaves_the_records_for_the_retry() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::os("delete the task", io::Error::other("denied"))));
    let mut machine = Machine::new(&fake, &[]);

    let failed = scm(&fake).task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine);

    assert!(failed.is_err());
    assert!(records_dir.exists());
}

/// A marker that cannot be placed fails the uninstall before any run is read: the task is registered and
/// disabled, and the failure says so.
#[skuld::test]
fn a_marker_that_cannot_be_placed_fails_the_uninstall_before_any_run_is_read() {
    let (_dir, records_dir, runs) = records();
    // A directory at the marker's name cannot be opened as the file.
    std::fs::create_dir(records_dir.with_file_name(format!("{}~removing", id()))).unwrap();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();

    let text = failed.to_string();
    assert!(
        text.contains("NOT deleted") && text.contains("It is disabled"),
        "{text}"
    );
    assert!(
        failed.report().to_string().contains("place the removal's marker"),
        "{}",
        failed.report()
    );
    assert_eq!(fake.calls(), [disable_call()], "no run is read, stopped or deleted");
    assert!(machine.held.is_empty());
    assert!(records_dir.exists());
    fake.finish();
}

/// The uninstall's own pin refused for a reason other than the marker fails it before any `Stop`, with the
/// task registered, disabled and under its marker, which the re-run finishes.
#[skuld::test]
fn a_pin_the_uninstall_cannot_take_fails_it_before_any_stop() {
    let (_dir, records_dir, runs) = records();
    let _denied = Denied::new(&records_dir);
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();

    let text = failed.to_string();
    assert!(
        text.contains("NOT deleted") && text.contains("It is disabled"),
        "{text}"
    );
    let report = failed.report().to_string();
    assert!(
        report.contains("`uninstall` was denied access to the run records of `nightly`"),
        "{report}"
    );
    assert_eq!(fake.calls(), [disable_call()], "no run is read, stopped or deleted");
    assert!(machine.held.is_empty());
    assert!(runs.is_closed().unwrap(), "the marker stands for the re-run");
    fake.finish();
}

/// What the world looks like at a step of an uninstall: a log that effects of the fake and of the machine
/// add to, with whether the removal's marker stands, how many pins the records hold and whether the
/// directory is there.
#[derive(Clone)]
struct Watch {
    log: Arc<Mutex<Vec<String>>>,
    dir: PathBuf,
    runs: Arc<Runs>,
}

impl Watch {
    /// A watch of the records directory `dir` under the temporary `%ProgramData%` `program_data`.
    fn new(program_data: &Path, dir: &Path) -> Self {
        Self {
            log: Arc::default(),
            runs: Arc::new(Runs::of(program_data, &id())),
            dir: dir.to_owned(),
        }
    }

    fn marker_stands(&self) -> bool {
        self.runs.is_closed().unwrap()
    }

    fn note(&self, step: &str) {
        let pins = if self.dir.exists() { pins_in(&self.dir) } else { 0 };
        let line = format!("{step}: marker={} pins={pins}", self.marker_stands());
        self.log.lock().unwrap().push(line);
    }

    fn at(&self, step: &'static str) -> impl FnOnce() + Send + use<> {
        let watch = self.clone();
        move || watch.note(step)
    }

    fn seen(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

/// The marker follows the disable and precedes the first read; the own pin is held until the runs are read,
/// and let go before the delete.
#[skuld::test]
fn uninstall_order() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let watch = Watch::new(dir.path(), &records_dir);
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .during(watch.at("disable"))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .during(watch.at("list"))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true))
        .during(watch.at("delete"));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    let exiting = watch.clone();
    machine.on_exit = Box::new(move |_| {
        exiting.note("exit");
        shim_writes(&shim, &record(1, RunOutcome::Stopped));
    });

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        watch.seen(),
        [
            "disable: marker=false pins=0",
            "list: marker=true pins=1",
            "exit: marker=true pins=1",
            "delete: marker=true pins=0",
        ]
    );
    assert!(
        !records_dir.exists() && !watch.marker_stands(),
        "the records and the marker go last"
    );
    fake.finish();
}

/// The uninstall's own pin is taken past its marker.
#[skuld::test]
fn uninstall_does_not_refuse_itself_with_its_own_marker() {
    let (_dir, records_dir, runs) = records();
    runs.close().unwrap();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(machine.waited.len(), 1, "the run was stopped and waited for");
    assert!(!records_dir.exists());
    fake.finish();
}

/// With no directory of records the marker is placed and no pin is taken; the run is stopped, and the marker
/// goes with the task.
#[skuld::test]
fn uninstall_of_a_task_with_no_records_directory_waits_for_nothing() {
    let (dir, records_dir, runs) = records();
    std::fs::remove_dir(&records_dir).unwrap();
    let spec = task_spec(Some("30 3 * * *"));
    let watch = Watch::new(dir.path(), &records_dir);
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .during(watch.at("list"))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        watch.seen(),
        ["list: marker=true pins=0"],
        "the marker stood at the first listing, though there is no directory"
    );
    assert_eq!(machine.waited.len(), 1);
    assert!(!watch.marker_stands(), "the marker is gone");
    assert!(!records_dir.exists(), "and no records directory was made");
    fake.finish();
}

/// A stop takes no second pin when the uninstall's own found no records: its marker would refuse it.
#[skuld::test]
fn uninstall_whose_own_pin_found_no_records_is_not_refused_by_its_marker() {
    let (_dir, records_dir, runs) = records();
    std::fs::remove_dir(&records_dir).unwrap();
    let spec = task_spec(Some("30 3 * * *"));
    let made = records_dir.clone();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .during(move || std::fs::create_dir(&made).unwrap())
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(fake.calls().contains(&Call::Stop(PATH.to_owned(), instance_id(1))));
    assert_eq!(machine.waited.len(), 1);
    fake.finish();
}

/// A task the disable found gone is not "disabled" when a later step fails: the error says it is not
/// installed and that records are what is left.
#[skuld::test]
fn a_failure_after_a_disable_that_found_the_task_gone_does_not_say_it_is_disabled() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_instances(Err(Error::os(
            "list the runs of `nightly`",
            io::Error::other("listing failed"),
        )));
    let mut machine = Machine::new(&fake, &[]);

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();

    let text = failed.to_string();
    assert!(
        !text.contains("NOT deleted") && !text.contains("It is disabled"),
        "{text}"
    );
    assert!(text.contains("`nightly` is not installed"), "{text}");
}

/// An uninstall that failed after its marker leaves the task disabled and the marker; the re-run finishes.
#[skuld::test]
fn rerun_after_an_incomplete_stop_finishes() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let watch = Watch::new(dir.path(), &records_dir);
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim_writes(&shim, &record(1, RunOutcome::IncompleteStop)));
    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap_err();
    let text = failed.to_string();
    assert!(
        text.contains("NOT deleted") && text.contains("It is disabled"),
        "{text}"
    );
    assert!(
        failed
            .report()
            .to_string()
            .contains("could not end the program's process tree"),
        "the incomplete stop: {}",
        failed.report()
    );
    assert!(
        !fake.calls().iter().any(|call| matches!(call, Call::Delete(..))),
        "the task is kept"
    );
    fake.finish();
    assert!(watch.marker_stands(), "the failed uninstall keeps its marker");
    assert_eq!(pins_in(&records_dir), 0, "and not its pin");

    let again = Fake::new();
    again
        .on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&again, &[]);
    scm(&again)
        .task_remove_with(&snapshot_of(&spec, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(!records_dir.exists() && !watch.marker_stands());
    again.finish();
}

/// A pin taken before the marker is still held when the task is deleted: nothing waits for it before the
/// delete. (That `remove_all` waits for it is `runs_tests`'s.)
#[skuld::test]
fn a_pin_taken_before_the_marker_is_held_across_the_delete() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let watch = Watch::new(dir.path(), &records_dir);
    let shim_pin = runs.hold().unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .during({
            let delete = watch.at("delete");
            move || {
                delete();
                drop(shim_pin);
            }
        });
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert_eq!(
        watch.seen(),
        ["delete: marker=true pins=1"],
        "the shim's pin was held at the delete"
    );
    assert!(!records_dir.exists() && !watch.marker_stands());
    fake.finish();
}

// The id that is gone -------------------------------------------------------------------------------------------------

/// An `uninstall` of an id that is not installed is how an uninstall that could not remove the records
/// is retried. The records are removed under the install lock, which an install that has just made
/// them holds.
#[skuld::test]
fn uninstall_of_an_id_that_is_absent_removes_the_records_under_the_install_lock() {
    let (_dir, records_dir, runs) = records();
    let fake = Fake::new();
    let locked = Cell::new(false);

    scm(&fake)
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || {
                assert!(records_dir.exists(), "the lock is taken before the records go");
                locked.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();

    assert!(locked.get());
    assert!(!records_dir.exists());
    assert_eq!(
        fake.calls(),
        [],
        "a manager with no provider has nothing to sweep, and reads no task"
    );
}

/// The sweep takes the install lock itself, so it runs after the records' removal has let the lock go.
#[skuld::test]
fn the_sweep_of_an_absent_id_runs_after_the_install_lock_is_released() {
    struct Lock<'a>(&'a Cell<bool>);
    impl Drop for Lock<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let (_dir, _records_dir, runs) = records();
    let released = Cell::new(false);
    let swept = Cell::new(false);

    scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || Ok(Lock(&released)),
            || {
                assert!(released.get(), "the lock was still held when the sweep began");
                swept.set(true);
                Ok(())
            },
        )
        .unwrap();

    assert!(swept.get());
}

/// An uninstall that died after its records' directory went, and before it removed its marker, leaves the
/// marker, which makes the id's `run` back off for good. The next uninstall of the id, which is absent,
/// takes it away, under the lock.
#[skuld::test]
fn uninstall_of_an_id_that_is_absent_clears_a_marker_left_beside_no_records() {
    let (_dir, records_dir, runs) = records();
    let marker = records_dir.with_file_name(format!("{}~removing", id()));
    std::fs::write(&marker, "").unwrap();
    std::fs::remove_dir(&records_dir).unwrap();
    let locked = Cell::new(false);

    scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || {
                locked.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();

    assert!(locked.get());
    assert!(!marker.exists());
}

/// With no records there is nothing to remove, and so no lock to take: a library caller that is not
/// elevated keeps getting `NotInstalled` for an id that never was.
#[skuld::test]
fn uninstall_of_an_id_with_no_records_takes_no_lock() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());

    scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || -> Result<()> { panic!("there is nothing to guard") },
            || Ok(()),
        )
        .unwrap();
}

/// A removal of the records of an id that is gone that fails says what it was removing, with the cause under
/// it: the id is not installed, so the path alone would not say what the command was doing.
#[skuld::test]
fn uninstall_of_an_id_that_is_absent_says_what_it_was_removing_when_the_removal_fails() {
    let (_dir, records_dir, runs) = records();
    let _held = crate::backend::scm::test_support::directory_held_open(&records_dir.join("stuck"));

    let failed = scm(&Fake::new())
        .uninstall_absent_with(&id(), &runs, RunInFlight::Stop, || -> Result<()> { Ok(()) }, || Ok(()))
        .unwrap_err();

    let text = failed.to_string();
    assert!(
        text.contains("`nightly` is not installed, and run records left for it could not be removed"),
        "{text}"
    );
    assert!(!text.contains("is deleted"), "nothing was deleted: {text}");
    assert!(
        !text.contains("earlier uninstall"),
        "the records may come from a failed install or a task deleted elsewhere: {text}"
    );
    assert!(
        text.contains("Re-run `goetia daemon uninstall nightly` to remove them"),
        "{text}"
    );
    assert!(failed.report().to_string().contains("stuck"), "{}", failed.report());
}

/// The shim sweep does not depend on the records: a removal that fails does not skip it, and a sweep that
/// fails too does not hide the records' failure.
#[skuld::test]
fn uninstall_of_an_id_that_is_absent_sweeps_even_when_the_records_cannot_be_removed() {
    let (_dir, records_dir, runs) = records();
    let _held = crate::backend::scm::test_support::directory_held_open(&records_dir.join("stuck"));
    let swept = Cell::new(false);

    let failed = scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || -> Result<()> { Ok(()) },
            || {
                swept.set(true);
                Ok(())
            },
        )
        .unwrap_err();

    assert!(swept.get(), "the sweep ran after the records failed");
    assert!(
        failed
            .to_string()
            .contains("run records left for it could not be removed"),
        "{failed}"
    );

    swept.set(false);
    let both = scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || -> Result<()> { Ok(()) },
            || {
                swept.set(true);
                Err(Error::os("sweep the shim files", io::Error::other("denied")))
            },
        )
        .unwrap_err();

    assert!(swept.get());
    let said = both.report().to_string();
    assert!(said.contains("stuck"), "the records' failure is kept: {said}");
    assert!(
        said.contains("sweep the shim files"),
        "the sweep's failure is kept: {said}"
    );
}

// Leaving the run -----------------------------------------------------------------------------------------------------

/// `uninstall --no-stop` from inside a run (its shim holds the pin): the task is deleted and the pin, records and
/// marker stay.
#[skuld::test]
fn leaving_a_run_whose_pin_is_held_deletes_the_task_and_keeps_marker_and_records() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let watch = Watch::new(dir.path(), &records_dir);
    let run_pin = runs.hold().unwrap();
    shim_writes(&Runs::of(dir.path(), &id()), &record(1, RunOutcome::Exited(0)));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_delete(Ok(true))
        .during(watch.at("delete"));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Leave, &mut machine)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [disable_call(), Call::Delete(PATH.to_owned())],
        "no listing of the runs and no stop; the disable and the delete are all there"
    );
    assert!(
        machine.held.is_empty() && machine.waited.is_empty(),
        "no process is held or waited for"
    );
    assert_eq!(
        watch.seen(),
        ["delete: marker=true pins=1"],
        "the marker stood at the delete, and the uninstall took no pin of its own"
    );
    assert!(watch.marker_stands(), "the marker stays");
    assert!(
        records_dir.join(format!("{}.json", instance_id(1).hex())).exists(),
        "the records stay"
    );
    assert_eq!(pins_in(&records_dir), 1, "the run's pin stays");
    drop(run_pin);
    fake.finish();
}

/// With no pin held there is no run to leave (or none that holds the records), and `--no-stop` removes
/// everything a plain uninstall does: no marker, no records.
#[skuld::test]
fn leaving_with_no_pin_held_leaves_nothing() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let watch = Watch::new(dir.path(), &records_dir);
    shim_writes(&Runs::of(dir.path(), &id()), &record(1, RunOutcome::Exited(0)));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Leave, &mut machine)
        .unwrap();

    assert_eq!(fake.calls(), [disable_call(), Call::Delete(PATH.to_owned())]);
    assert!(!records_dir.exists() && !watch.marker_stands(), "nothing is left");
    fake.finish();
}

/// `uninstall --no-stop` of an id that is absent does not wait for a held pin: the records of the earlier
/// `--no-stop` uninstall stay while the run lives, and the sweep, which stops nothing, runs all the same. Once the
/// run's pin is gone the same call removes the records and the marker.
#[skuld::test]
fn leaving_an_absent_id_does_not_wait_for_a_held_pin_and_removes_the_records_once_it_is_released() {
    let (dir, records_dir, runs) = records();
    shim_writes(&Runs::of(dir.path(), &id()), &record(1, RunOutcome::Exited(0)));
    let run_pin = runs.hold().unwrap();
    assert!(!runs.is_closed().unwrap(), "no marker stands before the call");
    let swept = Cell::new(0);
    let sweep = || {
        swept.set(swept.get() + 1);
        Ok(())
    };
    let scm = scm(&Fake::new());

    scm.uninstall_absent_with(&id(), &runs, RunInFlight::Leave, || Ok(()), sweep)
        .unwrap();

    assert_eq!(swept.get(), 1, "the sweep is no stop, and runs");
    assert!(
        records_dir.exists() && runs.is_closed().unwrap(),
        "the held records stay"
    );
    assert_eq!(pins_in(&records_dir), 1);

    drop(run_pin);
    scm.uninstall_absent_with(&id(), &runs, RunInFlight::Leave, || Ok(()), sweep)
        .unwrap();

    assert_eq!(swept.get(), 2);
    assert!(
        !records_dir.exists() && !runs.is_closed().unwrap(),
        "released, nothing is left"
    );
}

/// `Leave` waits for nothing, in one taking of the lock: with a held pin it ends there, and with no pin it makes one
/// pass and leaves what the pass could not remove (a record another program holds), with the marker, and says what
/// it left. It never enters the waiting loop, so there is no pause and the lock is taken exactly once.
#[skuld::test]
fn leaving_takes_the_lock_once_and_leaves_what_a_pass_could_not_remove() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let (dir, records_dir, runs) = records();
    shim_writes(&Runs::of(dir.path(), &id()), &record(1, RunOutcome::Exited(0)));
    shim_writes(&Runs::of(dir.path(), &id()), &record(2, RunOutcome::Exited(0)));
    let stuck = records_dir.join(format!("{}.json", instance_id(2).hex()));
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&stuck)
        .unwrap();
    let takings = Cell::new(0);
    let left = std::cell::RefCell::new(Vec::new());

    remove_records(
        &runs,
        RunInFlight::Leave,
        || {
            takings.set(takings.get() + 1);
            Ok(())
        },
        |path| left.borrow_mut().push(path.to_owned()),
    )
    .unwrap();

    assert_eq!(takings.get(), 1, "one taking, no pause");
    assert_eq!(
        left.borrow().as_slice(),
        std::slice::from_ref(&stuck),
        "it said what it left"
    );
    assert!(
        stuck.exists() && runs.is_closed().unwrap(),
        "the held record and the marker stay"
    );

    // A held pin ends it before any pass: nothing is deleted, and the pin is what is left.
    let (_dir, records_dir, runs) = records();
    let pin = runs.hold().unwrap();
    takings.set(0);
    left.borrow_mut().clear();
    remove_records(
        &runs,
        RunInFlight::Leave,
        || {
            takings.set(takings.get() + 1);
            Ok(())
        },
        |path| left.borrow_mut().push(path.to_owned()),
    )
    .unwrap();
    assert_eq!(takings.get(), 1);
    assert_eq!(left.borrow().len(), 1);
    assert_eq!(
        left.borrow()[0].parent(),
        Some(records_dir.as_path()),
        "the pin is named"
    );
    assert!(runs.is_closed().unwrap());
    drop(pin);
}

/// A plain uninstall of an absent id that waits does not carry its decision across the release of the lock: every
/// pass first checks that the removal's marker still stands. An `install` of the id that takes the lock during a pause
/// clears the marker and makes the id its own, and the next pass then removes nothing of it, places no marker, and
/// fails saying so.
#[skuld::test]
fn a_waiting_uninstall_whose_id_is_installed_again_removes_nothing_of_the_new_install() {
    let (dir, records_dir, runs) = records();
    let run_pin = std::cell::RefCell::new(Some(runs.hold().unwrap()));
    let takings = Cell::new(0);

    let failed = scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || -> Result<()> {
                takings.set(takings.get() + 1);
                // The third taking is the second pass: an install took the lock during the pause before it, after
                // the run had ended (its `Runs::ensure` ends in `reopen`).
                if takings.get() == 3 {
                    drop(run_pin.borrow_mut().take());
                    runs.reopen().unwrap();
                    std::fs::write(records_dir.join("new-install"), "kept").unwrap();
                }
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();

    assert!(matches!(failed.cause(), Error::InstalledAgain { .. }), "{failed:?}");
    assert_eq!(
        takings.get(),
        3,
        "the marker, a pass that waited, the pass that found the id taken"
    );
    assert!(
        records_dir.join("new-install").exists(),
        "nothing of the new install was removed"
    );
    assert!(!runs.is_closed().unwrap(), "and no marker was placed on it");
    drop(dir);
}

/// A plain `uninstall` of an absent id waits, with no bound, for a pin a left run holds; the install lock is a
/// machine-wide one, so it is taken for each pass and not held across the pauses: another install or uninstall
/// gets it while this one waits. The lock here is a counter that fails if it is taken while held, and the run's pin
/// is released at its third taking (the marker's, a first pass that finds the pin, a second that does not).
#[skuld::test]
fn a_plain_uninstall_of_an_absent_id_waits_for_a_held_pin_without_holding_the_install_lock() {
    struct Taken<'a>(&'a Cell<bool>);
    impl Drop for Taken<'_> {
        fn drop(&mut self) {
            self.0.set(false);
        }
    }
    let (dir, records_dir, runs) = records();
    shim_writes(&Runs::of(dir.path(), &id()), &record(1, RunOutcome::Exited(0)));
    let run_pin = std::cell::RefCell::new(Some(runs.hold().unwrap()));
    runs.close().unwrap();
    let (takings, held) = (Cell::new(0), Cell::new(false));
    let swept = Cell::new(false);

    scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || {
                assert!(!held.get(), "the lock was held across a pause");
                takings.set(takings.get() + 1);
                if takings.get() == 3 {
                    drop(run_pin.borrow_mut().take());
                }
                held.set(true);
                Ok(Taken(&held))
            },
            || {
                assert!(!held.get(), "the lock was still held when the sweep began");
                swept.set(true);
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(takings.get(), 3, "the marker, a pass that waited, a pass that finished");
    assert!(!held.get() && swept.get());
    assert!(
        !records_dir.exists() && !runs.is_closed().unwrap(),
        "released, nothing is left"
    );
}

/// An install lock that cannot be taken is the call's own error, once: the sweep takes the same lock, so it is
/// not tried, and the error is not wrapped as a failure to remove records.
#[skuld::test]
fn an_absent_id_uninstall_whose_install_lock_cannot_be_taken_reports_that_once() {
    let (_dir, records_dir, runs) = records();
    let swept = Cell::new(false);

    let failed = scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Stop,
            || -> Result<()> {
                Err(Error::ElevationRequired {
                    what: "take the install lock".into(),
                })
            },
            || {
                swept.set(true);
                Ok(())
            },
        )
        .unwrap_err();

    assert!(matches!(failed, Error::ElevationRequired { .. }), "{failed:?}");
    assert!(!swept.get(), "the sweep needs the same lock");
    assert!(records_dir.exists(), "nothing was removed");
}

/// With the choice and no pin held, the removal of an absent id's records is what it is without it.
#[skuld::test]
fn leaving_an_absent_id_with_no_pin_held_removes_the_records_under_the_install_lock() {
    let (_dir, records_dir, runs) = records();
    let locked = Cell::new(false);

    scm(&Fake::new())
        .uninstall_absent_with(
            &id(),
            &runs,
            RunInFlight::Leave,
            || {
                locked.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();

    assert!(locked.get());
    assert!(!records_dir.exists() && !runs.is_closed().unwrap());
}

/// A failed `--no-stop` uninstall asks for a retry that keeps `--no-stop`; one test per `Left` message.
#[skuld::test]
fn a_failed_leaving_uninstall_asks_for_its_retry_with_no_stop() {
    let retry = "Re-run `goetia daemon uninstall --no-stop nightly`";
    let spec = task_spec(Some("30 3 * * *"));

    // The disable failed: whether the task is still enabled is not known.
    let (_dir, _records_dir, runs) = records();
    let fake = Fake::new();
    fake.on_set_enabled(Err(Error::os("disable `nightly`", io::Error::other("refused"))));
    let mut machine = Machine::new(&fake, &[]);
    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Leave, &mut machine)
        .unwrap_err();
    let text = failed.to_string();
    assert!(
        text.contains("whether it is still enabled is not known") && text.contains(retry),
        "{text}"
    );

    // The delete failed: the task is registered and disabled.
    let (_dir, _records_dir, runs) = records();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_delete(Err(Error::os("delete `nightly`", io::Error::other("refused"))));
    let mut machine = Machine::new(&fake, &[]);
    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Leave, &mut machine)
        .unwrap_err();
    let text = failed.to_string();
    assert!(text.contains("It is disabled") && text.contains(retry), "{text}");

    // The records could not be removed after the delete.
    let (_dir, records_dir, runs) = records();
    let _held = crate::backend::scm::test_support::directory_held_open(&records_dir.join("stuck"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);
    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, RunInFlight::Leave, &mut machine)
        .unwrap_err();
    let text = failed.to_string();
    assert!(
        text.contains("is deleted, but its run records could not be removed") && text.contains(retry),
        "{text}"
    );

    // No task, and the records left for the id could not be removed.
    let (_dir, records_dir, runs) = records();
    let _held = crate::backend::scm::test_support::directory_held_open(&records_dir.join("stuck"));
    let failed = scm(&Fake::new())
        .uninstall_absent_with(&id(), &runs, RunInFlight::Leave, || -> Result<()> { Ok(()) }, || Ok(()))
        .unwrap_err();
    let text = failed.to_string();
    assert!(
        text.contains("run records left for it could not be removed") && text.contains(retry),
        "{text}"
    );
}

// Retype ==============================================================================================================

/// A crash between the removal and the creation leaves nothing at the id, which reads as absent, and
/// the install that runs again creates the task.
#[skuld::test]
fn a_crash_between_removal_and_creation_reads_as_absent_and_install_finishes() {
    let (_dir, _records_dir, runs) = records();
    let old = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_get(Ok(None))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);
    let mut machine = Machine::new(&fake, &[]);
    scm.task_remove_with(&snapshot_of(&old, false), &runs, RunInFlight::Stop, &mut machine)
        .unwrap();

    assert!(matches!(scm.task_read(&id()).unwrap(), ReadOutcome::Absent));
    let new = TaskXml::new(&old, task_host()).unwrap();
    let (_other, new_runs) = {
        let dir = tempfile::tempdir().unwrap();
        let runs = Runs::of(dir.path(), &id());
        // Made plainly: naming Administrators as an owner needs an elevated token. So is the default log
        // the install sets the permissions of.
        std::fs::create_dir_all(dir.path().join("Goetia").join("runs").join("nightly")).unwrap();
        let logs = crate::backend::scm::folder::logs_folder(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(crate::backend::scm::default_log::path_in(&logs, "nightly"), "").unwrap();
        (dir, runs)
    };
    scm.task_write_with(&old, &new, None, &new_runs, &mut machine, |_| {
        Ok(Prepared {
            principal: system(),
            password: None,
        })
    })
    .unwrap();

    let calls = fake.calls();
    assert!(
        matches!(calls.last(), Some(Call::Register { update: false, .. })),
        "{calls:?}"
    );
    fake.finish();
}
