//! The tests' waits for a service's state, over the product's own notification wait
//! ([`goetia::backend::scm::wait::wait_until`]). Unbounded, like every wait on a state SCM must
//! reach; a state it settles in that is not the one waited for fails the test with that state
//! instead of hanging it.

use std::ops::ControlFlow;

use goetia::backend::scm::wait::{QueriedState, SystemScmActor, Waited, WantState, wait_until};
use goetia::manager::Budget;

/// A notification actor on service `id`.
pub fn open(id: &str) -> SystemScmActor {
    SystemScmActor::open(id).unwrap_or_else(|e| panic!("open {id} to wait on its state: {e}"))
}

/// Returns once `id` reports `RUNNING`. A settled `STOPPED` or `PAUSED`, even the first state seen,
/// is a panic: the service was started, so it is not on its way to `RUNNING`.
pub fn wait_for_running(actor: &mut SystemScmActor, id: &str) {
    let waited = wait_until(
        actor,
        WantState::Running,
        WantState::Running,
        QueriedState::Running,
        Budget::Unbounded.start(),
        |sighting| match sighting.status.state {
            state @ (QueriedState::Paused | QueriedState::Stopped) => {
                panic!("waiting for RUNNING, {id} reached {state:?}")
            }
            _ => Ok(ControlFlow::Continue(())),
        },
    )
    .unwrap_or_else(|e| panic!("wait for {id} to report RUNNING: {e}"));
    assert_eq!(
        waited,
        Waited::Confirmed,
        "an unbounded wait for {id} to report RUNNING expired"
    );
}

/// Returns once `id` reports `STOPPED`, after a stop was sent. The first reading may still be
/// `RUNNING`, as a control's own status shows the state it left; once a notification has been
/// delivered, a settled state other than `STOPPED` (a recovery restart's `RUNNING`) is a panic.
pub fn wait_for_stopped(actor: &mut SystemScmActor, id: &str) {
    let waited = wait_until(
        actor,
        WantState::Stopped,
        WantState::StoppedOnly,
        QueriedState::Stopped,
        Budget::Unbounded.start(),
        |sighting| match sighting.status.state {
            _ if !sighting.pending_delivered => Ok(ControlFlow::Continue(())),
            QueriedState::StopPending => Ok(ControlFlow::Continue(())),
            state => panic!("waiting for STOPPED, {id} reached {state:?}"),
        },
    )
    .unwrap_or_else(|e| panic!("wait for {id} to reach STOPPED: {e}"));
    assert_eq!(waited, Waited::Confirmed, "an unbounded wait for {id} to stop expired");
}
