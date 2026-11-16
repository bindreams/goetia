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
    remove_task_with(fake, &id(), &mut machine);
    machine
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
