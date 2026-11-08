//! The tests' waits for a service's state, over the product's own notification wait
//! ([`goetia::backend::scm::wait::wait_until`]). Unbounded, like every wait on a state SCM must
//! reach; a state it settles in after the wait began that is not the one waited for fails the test
//! with that state instead of hanging it. After a STOP only sent, a first read other than
//! `STOP_PENDING` or `STOPPED` keeps waiting: the stop masks deliver neither `RUNNING` nor `PAUSED`,
//! so a service that never leaves the state it was in hangs the wait.

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
        WantState::RunningPausedOrStopped,
        WantState::RunningPausedOrStopped,
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

/// Returns once `id` reports `STOPPED`, after a STOP was sent and nothing of it has shown yet: a
/// first read other than `STOPPED` waits (the service may still read `RUNNING`). Once a notification
/// was delivered, a state other than `STOP_PENDING` is a panic: the service is running again after it
/// began to stop (a recovery restart).
pub fn wait_for_stopped_after_stop(actor: &mut SystemScmActor, id: &str) {
    wait_for_stopped(actor, id, WantState::Stopped, true);
}

/// Returns once `id` reports `STOPPED`, after `STOP_PENDING` was seen: a settled state other than
/// `STOP_PENDING` or `STOPPED`, even the first read, is a panic.
pub fn wait_for_stopped_after_pending(actor: &mut SystemScmActor, id: &str) {
    wait_for_stopped(actor, id, WantState::StoppedOnly, false);
}

fn wait_for_stopped(actor: &mut SystemScmActor, id: &str, first: WantState, first_read_waits: bool) {
    let waited = wait_until(
        actor,
        first,
        WantState::StoppedOnly,
        QueriedState::Stopped,
        Budget::Unbounded.start(),
        |sighting| match sighting.status.state {
            _ if first_read_waits && !sighting.pending_delivered => Ok(ControlFlow::Continue(())),
            QueriedState::StopPending => Ok(ControlFlow::Continue(())),
            state => panic!("waiting for STOPPED, {id} reached {state:?}"),
        },
    )
    .unwrap_or_else(|e| panic!("wait for {id} to reach STOPPED: {e}"));
    assert_eq!(waited, Waited::Confirmed, "an unbounded wait for {id} to stop expired");
}
