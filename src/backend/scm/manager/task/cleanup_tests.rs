//! What a test's guard removes, over the table-driven fake of Task Scheduler.

use super::super::support::{Machine, PATH, instance, instance_id};
use super::*;
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::error::Error;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

fn removed(fake: &Fake, alive: &[u32]) -> Machine {
    let mut machine = Machine::new(fake, alive);
    remove_task_with(fake, &id(), &mut machine, || Ok(()));
    machine
}

/// What the install lock does in a test: notes how many calls the scheduler had had when it was taken and
/// when it was let go.
struct NotedLock {
    fake: Fake,
    released_at: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
}

impl Drop for NotedLock {
    fn drop(&mut self) {
        *self.released_at.lock().unwrap() = Some(self.fake.calls().len());
    }
}

/// The delete, which ends in the removal of the empty `\Goetia` folder, is made under the install lock, as
/// an install is made under it: a guard of an unrelated test that deletes a task that is not there must not
/// remove the folder between an install's folder and its registration. The stop is not made under it: it
/// waits for a shim, and an install would wait on the lock the while.
#[skuld::test]
fn the_delete_is_made_under_the_install_lock_and_the_stop_is_not() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let released_at = std::sync::Arc::new(std::sync::Mutex::new(None));
    let taken_at = std::cell::Cell::new(None);
    let mut machine = Machine::new(&fake, &[]);

    remove_task_with(&fake, &id(), &mut machine, || {
        taken_at.set(Some(fake.calls().len()));
        Ok(NotedLock {
            fake: fake.clone(),
            released_at: released_at.clone(),
        })
    });

    assert_eq!(
        taken_at.get(),
        Some(2),
        "taken after the disable and the stop, before the delete"
    );
    assert_eq!(*released_at.lock().unwrap(), Some(3), "let go after the delete");
    fake.finish();
}

/// A lock that cannot be taken is warned of and the task is deleted all the same: a guard leaves nothing
/// behind for want of it.
#[skuld::test]
fn a_lock_that_cannot_be_taken_does_not_stop_the_delete() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let mut machine = Machine::new(&fake, &[]);

    remove_task_with(&fake, &id(), &mut machine, || -> Result<()> {
        Err(Error::os("take the lock", std::io::Error::other("denied")))
    });

    assert!(fake.calls().contains(&Call::Delete(PATH.to_owned())));
    fake.finish();
}

/// A scheduled fire between the guard's stop and its delete would start a run the delete leaves behind,
/// so the task is disabled first, which takes no password and so works for any account.
#[skuld::test]
fn a_task_is_disabled_before_it_is_deleted() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));

    removed(&fake, &[]);

    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::Instances(PATH.to_owned()),
            Call::Delete(PATH.to_owned())
        ]
    );
    fake.finish();
}

/// A run in flight is stopped, and the shim's process waited for, before the task is deleted: a delete
/// under a live shim would leave it running with nothing to stop it.
#[skuld::test]
fn a_run_is_stopped_and_its_process_waited_for_before_the_task_is_deleted() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_stop(Ok(()))
        .on_delete(Ok(true));

    let machine = removed(&fake, &[4242]);

    assert_eq!(
        fake.calls().last(),
        Some(&Call::Delete(PATH.to_owned())),
        "the delete comes last"
    );
    assert!(fake.calls().contains(&Call::Stop(PATH.to_owned(), instance_id(1))));
    assert_eq!(
        machine.waited,
        [(4242, 5)],
        "the process is waited for after the stop, before the delete"
    );
    fake.finish();
}

/// A run that cannot be stopped is no reason to leave the task: the guard goes on to delete it.
#[skuld::test]
fn a_stop_that_fails_does_not_stop_the_delete() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Err(Error::os("list the runs", std::io::Error::other("denied"))))
        .on_delete(Ok(true));

    removed(&fake, &[]);

    assert!(fake.calls().contains(&Call::Delete(PATH.to_owned())));
    fake.finish();
}

/// A delete that took the task and could not remove the folder it left empty has done what the guard is
/// for: it is not asked again, and nothing is warned of.
#[skuld::test]
fn a_folder_left_after_the_delete_is_no_failed_delete() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::TaskFolderKept {
            task: PATH.to_owned(),
            folder: r"\Goetia".to_owned(),
            source: Box::new(Error::os("remove", std::io::Error::other("in use"))),
        }));

    removed(&fake, &[]);

    let deletes = fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Delete(_)))
        .count();
    assert_eq!(deletes, 1, "not retried");
    fake.finish();
}

/// A disable that failed for any reason but a task that is not there is warned of, plainly: the task may
/// still fire while the guard waits for its runs. Nothing is left behind by it, so it is not the "left
/// behind" warning of a delete that gave up.
#[skuld::test]
fn a_disable_that_failed_is_warned_of_unless_the_task_is_not_there() {
    let path = TaskPath::of(&id());

    assert_eq!(disable_warning(&path, Ok(())), None);
    assert_eq!(
        disable_warning(&path, Err(Error::NotInstalled { id: "nightly".into() })),
        None
    );
    let told = disable_warning(
        &path,
        Err(Error::os("disable the task", std::io::Error::other("denied"))),
    )
    .expect("a failure is told");
    assert!(told.starts_with("warning: disable the task"), "{told}");
    assert!(!told.contains("left behind"), "{told}");
    assert!(told.contains(PATH) && told.contains("denied"), "{told}");
}

/// Disabling is best effort: a task that is not there, or that cannot be disabled, is still deleted, and
/// nothing panics.
#[skuld::test]
fn a_disable_that_cannot_be_made_does_not_stop_the_delete() {
    let fake = Fake::new();
    fake.on_set_enabled(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_instances(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_delete(Ok(false))
        .on_set_enabled(Err(Error::os("disable the task", std::io::Error::other("denied"))))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));

    removed(&fake, &[]);
    removed(&fake, &[]);

    let deletes = fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Delete(_)))
        .count();
    assert_eq!(deletes, 2);
    fake.finish();
}
