//! The real Task Scheduler behind [`Com`]. Every test registers real tasks, so all of them are in the
//! `scm` group and run on CI. They hold the machine-wide install lock, as every verb that registers
//! or removes a task does: the tests of one binary run as separate processes, and the folder
//! `\Goetia` is created and removed with its tasks.

use super::*;
use crate::backend::scm::manager::task::cleanup::remove_task;
use crate::backend::scm::shim::lock::InstallLock;
use crate::error::Error;
use crate::spec::Id;
use crate::testing::groups::{Group, scm_group};

const FOREIGN_FOLDER: &str = r"\GoetiaForeignFolder";

/// The task XML of an action that runs `ping.exe` for `count` echoes, as SYSTEM.
fn xml(count: u32, description: &str, data: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>{description}</Description></RegistrationInfo>
  <Principals><Principal id="Author"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><Enabled>true</Enabled></Settings>
  <Actions Context="Author"><Exec><Command>ping.exe</Command><Arguments>-n {count} 127.0.0.1</Arguments></Exec></Actions>
  <Data>{data}</Data>
</Task>"#
    )
}

fn system() -> TaskAccount {
    TaskAccount::Service("SYSTEM".to_owned())
}

/// A task of the test's, removed when the guard drops, and at the start in case an earlier run left
/// it. The guard exists before the task does.
struct TaskGuard(Id);

impl TaskGuard {
    fn new(id: &str) -> (Self, TaskPath) {
        let id = Id::try_from(id).unwrap();
        remove_task(&id);
        let path = TaskPath::of(&id);
        (Self(id), path)
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        remove_task(&self.0);
    }
}

/// A task registered outside `\Goetia`, removed on drop; its folder goes with it.
struct ForeignGuard(String);

impl ForeignGuard {
    fn new(name: &str) -> Self {
        let guard = Self(format!(r"{FOREIGN_FOLDER}\{name}"));
        let _ = Com.delete_at(&guard.0);
        guard
    }
}

impl Drop for ForeignGuard {
    fn drop(&mut self) {
        let _ = Com.delete_at(&self.0);
    }
}

/// Ends every instance of `path` on drop, so a failing assert leaves no process running.
struct StopGuard<'a>(&'a TaskPath);

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        for instance in Com.instances(self.0).unwrap_or_default() {
            let _ = Com.stop(self.0, &instance.id);
        }
    }
}

/// Whether the folder `path` exists.
fn folder_exists(path: &str) -> bool {
    let session = Session::open().unwrap();
    session.folder(path).is_ok()
}

#[skuld::test]
fn a_task_registers_reads_back_and_deletes(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_guard, path) = TaskGuard::new("goetia-com-roundtrip");

    assert!(Com.get(&path).unwrap().is_none());
    let registered = Com
        .register(&path, &xml(1, "the first", "the data"), &system(), None, false)
        .unwrap();
    assert_eq!(registered, Registration::Created);

    let read = Com.get(&path).unwrap().expect("the task is registered");
    assert!(read.xml.contains("ping.exe"), "{}", read.xml);
    assert_eq!(read.data, "the data");
    assert!(Com.names().unwrap().contains(&"goetia-com-roundtrip".to_owned()));
    assert_eq!(Com.instances(&path).unwrap(), vec![]);
    // M1: a task that never ran has no last run, and one with no trigger has no next run.
    let info = Com.info(&path).unwrap();
    assert_eq!((info.last_run, info.next_run), (None, None), "{info:?}");

    assert!(Com.delete(&path).unwrap());
    assert!(!Com.delete(&path).unwrap());
    assert!(Com.get(&path).unwrap().is_none());
}

#[skuld::test]
fn an_update_replaces_the_registered_task(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_guard, path) = TaskGuard::new("goetia-com-update");
    Com.register(&path, &xml(1, "the first", "one"), &system(), None, false)
        .unwrap();

    let updated = Com
        .register(&path, &xml(2, "the second", "two"), &system(), None, true)
        .unwrap();

    assert_eq!(updated, Registration::Created);
    let read = Com.get(&path).unwrap().unwrap();
    assert_eq!(read.data, "two");
    assert!(read.xml.contains("-n 2 "), "{}", read.xml);
}

// M10: a create of an existing path and an update of a missing one are the two codes `Raced` stands for.
#[skuld::test]
fn register_of_an_existing_path_is_raced_and_update_of_a_missing_one_is_raced(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_guard, path) = TaskGuard::new("goetia-com-raced");
    let (_other, other_path) = TaskGuard::new("goetia-com-raced-other");
    let text = xml(1, "raced", "");

    // The folder does not exist yet.
    assert_eq!(
        Com.register(&path, &text, &system(), None, true).unwrap(),
        Registration::Raced
    );
    assert!(!folder_exists(TASK_FOLDER), "an update creates no folder");

    assert_eq!(
        Com.register(&path, &text, &system(), None, false).unwrap(),
        Registration::Created
    );
    assert_eq!(
        Com.register(&path, &text, &system(), None, false).unwrap(),
        Registration::Raced
    );

    // The folder exists, the task does not.
    assert_eq!(
        Com.register(&other_path, &text, &system(), None, true).unwrap(),
        Registration::Raced
    );
    assert!(Com.get(&other_path).unwrap().is_none());
}

#[skuld::test]
fn names_lists_only_the_goetia_folder(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_guard, path) = TaskGuard::new("goetia-com-names");
    let foreign = ForeignGuard::new("goetia-com-foreign");
    assert_eq!(Com.names().unwrap(), Vec::<String>::new());
    Com.register(&path, &xml(1, "ours", ""), &system(), None, false)
        .unwrap();
    Com.register_at(&foreign.0, &xml(1, "not ours", ""), &system(), None, false)
        .unwrap();

    assert_eq!(Com.names().unwrap(), vec!["goetia-com-names".to_owned()]);
}

#[skuld::test]
fn deleting_the_last_task_removes_the_folder(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_first_guard, first) = TaskGuard::new("goetia-com-last-1");
    let (_second_guard, second) = TaskGuard::new("goetia-com-last-2");
    let text = xml(1, "last", "");
    Com.register(&first, &text, &system(), None, false).unwrap();
    Com.register(&second, &text, &system(), None, false).unwrap();

    assert!(Com.delete(&first).unwrap());
    assert!(folder_exists(TASK_FOLDER), "a task is left in it");
    assert!(Com.delete(&second).unwrap());

    assert!(!folder_exists(TASK_FOLDER));
    assert_eq!(Com.names().unwrap(), Vec::<String>::new());
}

#[skuld::test]
fn a_run_is_listed_and_a_stop_ends_it(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_guard, path) = TaskGuard::new("goetia-com-run");
    let _stop = StopGuard(&path);
    // A run of an hour, ended by the stop.
    Com.register(&path, &xml(3600, "runs", ""), &system(), None, false)
        .unwrap();

    let started = Com.run(&path).unwrap();

    let listed = Com.instances(&path).unwrap();
    assert!(
        listed.iter().any(|i| i.id == started.id),
        "{listed:?} lacks {started:?}"
    );
    Com.stop(&path, &started.id).unwrap();
    // The listing drops the instance once the engine has ended it: the real condition, re-read.
    while !Com.instances(&path).unwrap().is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // A stop of an instance no longer listed is done.
    Com.stop(&path, &started.id).unwrap();
}

#[skuld::test]
fn a_query_of_a_missing_task_is_not_installed(#[fixture(scm_group)] _group: &Group) {
    let _lock = InstallLock::acquire().unwrap();
    let (_guard, path) = TaskGuard::new("goetia-com-missing");

    for result in [
        Com.instances(&path).map(drop),
        Com.info(&path).map(drop),
        Com.run(&path).map(drop),
    ] {
        assert!(matches!(result, Err(Error::NotInstalled { .. })), "{result:?}");
    }
}
