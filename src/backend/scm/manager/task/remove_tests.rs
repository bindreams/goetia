//! Removing a task over the table-driven fake of Task Scheduler, with the run records in a temporary
//! directory and the machine's processes a table the tests state: which are alive, which can be held,
//! and what a held one does when it is waited for (a shim writing its record).

use std::io;
use std::path::PathBuf;

use tempfile::TempDir;
use windows::Win32::Foundation::SCHED_E_TASK_NOT_RUNNING;

use super::*;
use crate::backend::ReadOutcome;
use crate::backend::scm::instance::{InstanceId, TaskAccount};
use crate::backend::scm::manager::task::api::{Instance, Registration};
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::manager::task::write::Prepared;
use crate::backend::scm::sid::system;
use crate::backend::scm::test_support::{registered_task, task_host, task_spec, task_spec_as};
use crate::blob::Blob;
use crate::elevation::test_support::process_user_sid;
use crate::spec::{DaemonSpec, User};

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

const PATH: &str = r"\Goetia\nightly";

fn instance_id(n: u8) -> InstanceId {
    InstanceId::parse(&format!("{n:032x}")).unwrap()
}

fn instance(n: u8, pid: u32) -> Instance {
    Instance {
        id: instance_id(n),
        pid,
    }
}

fn scm(fake: &Fake) -> Scm {
    Scm::new().with_tasks(Box::new(fake.clone()))
}

fn snapshot_of(spec: &DaemonSpec, armed: bool) -> TaskSnapshot {
    let blob = Blob {
        schema: 1,
        version: crate::version().to_string(),
        spec: spec.clone(),
    };
    TaskSnapshot::new(id(), Ok(blob), registered_task(spec, armed))
}

/// The task's record directory in a temporary `%ProgramData%`, made as an install makes it, for the
/// account this test runs as (the test removes it).
fn records() -> (TempDir, PathBuf, Runs) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    runs.ensure(&process_user_sid()).unwrap();
    let path = dir.path().join("Goetia").join("runs").join("nightly");
    (dir, path, runs)
}

fn record(n: u8, outcome: RunOutcome) -> RunRecord {
    RunRecord {
        instance: instance_id(n),
        started: 1,
        outcome,
    }
}

/// The machine's processes, as the test says: `alive` pids run, `holdable` ones can be opened, and
/// waiting for one runs `on_exit` (what the shim does as it ends) and notes how many calls the
/// scheduler had had by then.
struct Machine {
    fake: Fake,
    alive: Vec<u32>,
    holdable: Vec<u32>,
    held: Vec<u32>,
    waited: Vec<(u32, usize)>,
    on_exit: Box<dyn FnMut(u32)>,
}

impl Machine {
    fn new(fake: &Fake, alive: &[u32]) -> Self {
        Self {
            fake: fake.clone(),
            alive: alive.to_vec(),
            holdable: alive.to_vec(),
            held: Vec::new(),
            waited: Vec::new(),
            on_exit: Box::new(|_| {}),
        }
    }
}

impl Processes for Machine {
    type Held = u32;

    fn alive(&mut self, pid: u32) -> io::Result<Liveness> {
        Ok(if self.alive.contains(&pid) {
            Liveness::Alive
        } else {
            Liveness::Gone
        })
    }

    fn hold(&mut self, pid: u32) -> io::Result<Option<u32>> {
        self.held.push(pid);
        Ok(self.holdable.contains(&pid).then_some(pid))
    }

    fn wait_for_exit(&mut self, held: u32) -> io::Result<()> {
        self.waited.push((held, self.fake.calls().len()));
        (self.on_exit)(held);
        Ok(())
    }
}

fn no_password() -> Result<Option<String>> {
    panic!("a service account reads no password")
}

fn disarmed_xml(spec: &DaemonSpec) -> String {
    TaskXml::with_armed(&registered_task(spec, true).xml, false).unwrap()
}

fn disarm_call(spec: &DaemonSpec) -> Call {
    Call::Register {
        path: PATH.to_owned(),
        xml: disarmed_xml(spec),
        who: TaskAccount::Service("S-1-5-18".to_owned()),
        password: None,
        update: true,
    }
}

// Uninstall ===========================================================================================================

#[skuld::test]
fn uninstall_of_a_running_task_stops_it_then_deletes_it_and_its_records() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim.write(&record(1, RunOutcome::Stopped)).unwrap());

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, &mut machine, no_password)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [
            disarm_call(&spec),
            Call::Instances(PATH.to_owned()),
            Call::Instances(PATH.to_owned()),
            Call::Stop(PATH.to_owned(), instance_id(1)),
            Call::Delete(PATH.to_owned()),
        ],
        "disarmed first, so nothing starts another run, then the run is stopped and the task deleted"
    );
    assert_eq!(machine.held, [4242], "the run's process is held before it is stopped");
    assert_eq!(
        machine.waited,
        [(4242, 4)],
        "its exit is waited for after the stop and before the delete"
    );
    assert!(!records_dir.exists(), "the records go with the task");
    fake.finish();
}

#[skuld::test]
fn uninstall_of_an_idle_disarmed_task_writes_nothing_before_it_deletes() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new())).on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [Call::Instances(PATH.to_owned()), Call::Delete(PATH.to_owned())]
    );
    assert!(!records_dir.exists());
}

/// What the shim could not end is a daemon that may run: the uninstall fails, with the task registered
/// and disarmed, so that re-running it is the retry and nothing starts it meanwhile.
#[skuld::test]
fn uninstall_of_an_incomplete_stop_leaves_the_task_disarmed() {
    let (dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim.write(&record(1, RunOutcome::IncompleteStop)).unwrap());

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, &mut machine, no_password)
        .unwrap_err();

    let text = failed.to_string();
    assert!(text.contains("NOT deleted") && text.contains("disarmed"), "{text}");
    assert!(
        text.contains("goetia daemon uninstall nightly"),
        "the retry is named: {text}"
    );
    assert_eq!(
        fake.calls(),
        [
            disarm_call(&spec),
            Call::Instances(PATH.to_owned()),
            Call::Instances(PATH.to_owned()),
            Call::Stop(PATH.to_owned(), instance_id(1)),
        ],
        "no delete"
    );
    assert!(records_dir.exists());
    fake.finish();
}

/// Disarming is a registration, which a password-logon task makes only with its password (M3), so an
/// armed one is refused before any write; a disarmed one is never registered and needs none.
#[skuld::test]
fn uninstall_of_an_armed_real_user_task_without_its_password_is_refused_before_any_write() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    let mut machine = Machine::new(&fake, &[]);

    let refused = scm(&fake).task_remove_with(&snapshot_of(&spec, true), &runs, &mut machine, || Ok(None));

    assert!(matches!(refused, Err(Error::PasswordRequired { .. })), "{refused:?}");
    assert_eq!(fake.calls(), []);
    assert!(records_dir.exists(), "nothing was removed");
}

#[skuld::test]
fn uninstall_of_an_armed_real_user_task_registers_it_disarmed_with_its_password() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, &mut machine, || {
            Ok(Some("secret".to_owned()))
        })
        .unwrap();

    let calls = fake.calls();
    let Call::Register {
        who, password, update, ..
    } = &calls[0]
    else {
        panic!("the disarm registers: {calls:?}");
    };
    assert_eq!(who, &TaskAccount::User("alice".to_owned()));
    assert_eq!(password.as_deref(), Some("secret"));
    assert!(update);
}

#[skuld::test]
fn uninstall_of_a_disarmed_real_user_task_needs_no_password() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new())).on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, || {
            panic!("a task that is disarmed is not registered, so it needs no password")
        })
        .unwrap();

    assert!(!records_dir.exists());
}

#[skuld::test]
fn every_live_instance_is_stopped() {
    let (_dir, _records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let both = || Ok(vec![instance(1, 100), instance(2, 200)]);
    let fake = Fake::new();
    fake.on_instances(both())
        .on_instances(both())
        .on_stop(Ok(()))
        .on_instances(both())
        .on_stop(Ok(()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[100, 200]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password)
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
    fake.on_instances(Ok(vec![instance(1, 4242)])).on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password)
        .unwrap();

    assert_eq!(
        fake.calls(),
        [Call::Instances(PATH.to_owned()), Call::Delete(PATH.to_owned())]
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
    fake.on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password)
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
    fake.on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Err(Error::os(
            "stop an instance",
            windows::core::Error::from(SCHED_E_TASK_NOT_RUNNING),
        )))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[4242]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password)
        .unwrap();

    assert_eq!(machine.waited.len(), 1);
}

#[skuld::test]
fn any_other_failure_of_a_stop_fails_the_uninstall_and_deletes_nothing() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Err(Error::os("stop an instance", io::Error::other("denied"))));
    let mut machine = Machine::new(&fake, &[4242]);

    let failed = scm(&fake).task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password);

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
    fake.on_instances(Ok(vec![instance(1, 4242)]));
    struct Denied;
    impl Processes for Denied {
        type Held = ();
        fn alive(&mut self, _: u32) -> io::Result<Liveness> {
            Err(io::Error::from_raw_os_error(5))
        }
        fn hold(&mut self, _: u32) -> io::Result<Option<()>> {
            unreachable!("a process that cannot be judged is not held")
        }
        fn wait_for_exit(&mut self, _: ()) -> io::Result<()> {
            unreachable!()
        }
    }

    let failed = scm(&fake)
        .task_remove_with(&snapshot_of(&spec, false), &runs, &mut Denied, no_password)
        .unwrap_err();

    let report = failed.report().to_string();
    assert!(report.contains("denied access"), "{report}");
    assert_eq!(fake.calls(), [Call::Instances(PATH.to_owned())]);
}

#[skuld::test]
fn a_task_that_went_meanwhile_is_uninstalled_all_the_same() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Raced))
        .on_instances(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_delete(Ok(false));
    let mut machine = Machine::new(&fake, &[]);

    scm(&fake)
        .task_remove_with(&snapshot_of(&spec, true), &runs, &mut machine, no_password)
        .unwrap();

    assert!(!records_dir.exists());
}

#[skuld::test]
fn a_failed_delete_leaves_the_records_for_the_retry() {
    let (_dir, records_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::os("delete the task", io::Error::other("denied"))));
    let mut machine = Machine::new(&fake, &[]);

    let failed = scm(&fake).task_remove_with(&snapshot_of(&spec, false), &runs, &mut machine, no_password);

    assert!(failed.is_err());
    assert!(records_dir.exists());
}

// The id that is gone -------------------------------------------------------------------------------------------------

/// An `uninstall` of an id that is not installed is how an uninstall that could not remove the records
/// is retried, and it sweeps as a service's does.
#[skuld::test]
fn uninstall_of_an_id_that_is_absent_removes_the_records_and_sweeps_as_a_service_does() {
    let (_dir, records_dir, runs) = records();
    let fake = Fake::new();

    scm(&fake).uninstall_absent_with(&runs).unwrap();

    assert!(!records_dir.exists());
    assert_eq!(
        fake.calls(),
        [],
        "a manager with no provider has nothing to sweep, and reads no task"
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
    fake.on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_get(Ok(None))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);
    let mut machine = Machine::new(&fake, &[]);
    scm.task_remove_with(&snapshot_of(&old, false), &runs, &mut machine, no_password)
        .unwrap();

    assert!(matches!(scm.task_read(&id()).unwrap(), ReadOutcome::Absent));
    let host = task_host();
    let new = TaskXml::new(&old, host).unwrap();
    let (_other, new_runs) = {
        let dir = tempfile::tempdir().unwrap();
        let runs = Runs::of(dir.path(), &id());
        (dir, runs)
    };
    scm.task_write_with(&old, &new, None, &new_runs, |_| {
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
