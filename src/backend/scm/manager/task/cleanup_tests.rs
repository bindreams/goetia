//! What a test's guard removes, over the table-driven fake of Task Scheduler.

use super::*;
use crate::backend::scm::manager::task::api::{Registered, Registration};
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::test_support::{registered_task, task_spec, task_spec_as};
use crate::error::Error;
use crate::spec::User;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

const PATH: &str = r"\Goetia\nightly";

fn registered(armed: bool) -> Registered {
    registered_task(&task_spec(Some("30 3 * * *")), armed)
}

/// A scheduled fire between the guard's stop and its delete would start a run the delete leaves behind,
/// so an armed task is disarmed first.
#[skuld::test]
fn an_armed_task_is_disarmed_before_it_is_deleted() {
    let fake = Fake::new();
    let armed = registered(true);
    fake.on_get(Ok(Some(armed.clone())))
        .on_register(Ok(Registration::Created))
        .on_delete(Ok(true));

    remove_task_with(&fake, &id());

    let calls = fake.calls();
    assert_eq!(calls[0], Call::Get(PATH.to_owned()));
    let Call::Register { xml, update, .. } = &calls[1] else {
        panic!("the task is registered disarmed: {calls:?}");
    };
    assert!(*update && !TaskXml::armed(xml), "{xml}");
    assert_eq!(calls[2], Call::Delete(PATH.to_owned()));
    fake.finish();
}

#[skuld::test]
fn a_task_that_is_not_armed_or_not_there_is_only_deleted() {
    let fake = Fake::new();
    fake.on_get(Ok(Some(registered(false))))
        .on_delete(Ok(true))
        .on_get(Ok(None))
        .on_delete(Ok(false));

    remove_task_with(&fake, &id());
    remove_task_with(&fake, &id());

    assert_eq!(
        fake.calls(),
        [
            Call::Get(PATH.to_owned()),
            Call::Delete(PATH.to_owned()),
            Call::Get(PATH.to_owned()),
            Call::Delete(PATH.to_owned()),
        ]
    );
}

/// Disarming is best effort: a cleanup that cannot disarm (a read that failed, a real user's task, which
/// takes its password to be registered again) still deletes, and never panics.
#[skuld::test]
fn a_disarm_that_cannot_be_made_does_not_stop_the_delete() {
    let fake = Fake::new();
    let user = registered_task(&task_spec_as(User::Name("alice".into()), Some("30 3 * * *")), true);
    fake.on_get(Err(Error::os("read the task", std::io::Error::other("denied"))))
        .on_delete(Ok(true))
        .on_get(Ok(Some(user)))
        .on_register(Err(Error::os("register the task", std::io::Error::other("denied"))))
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
