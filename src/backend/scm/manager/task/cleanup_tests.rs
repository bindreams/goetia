//! What a test's guard removes, over the table-driven fake of Task Scheduler.

use super::*;
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::error::Error;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

const PATH: &str = r"\Goetia\nightly";

/// A scheduled fire between the guard's stop and its delete would start a run the delete leaves behind,
/// so the task is disabled first, which takes no password and so works for any account.
#[skuld::test]
fn a_task_is_disabled_before_it_is_deleted() {
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_delete(Ok(true));

    remove_task_with(&fake, &id());

    assert_eq!(
        fake.calls(),
        [Call::SetEnabled(PATH.to_owned(), false), Call::Delete(PATH.to_owned())]
    );
    fake.finish();
}

/// Disabling is best effort: a task that is not there, or that cannot be disabled, is still deleted, and
/// nothing panics.
#[skuld::test]
fn a_disable_that_cannot_be_made_does_not_stop_the_delete() {
    let fake = Fake::new();
    fake.on_set_enabled(Err(Error::NotInstalled { id: "nightly".into() }))
        .on_delete(Ok(false))
        .on_set_enabled(Err(Error::os("disable the task", std::io::Error::other("denied"))))
        .on_delete(Ok(true));

    remove_task_with(&fake, &id());
    remove_task_with(&fake, &id());

    let deletes = fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Delete(_)))
        .count();
    assert_eq!(deletes, 2);
    fake.finish();
}
