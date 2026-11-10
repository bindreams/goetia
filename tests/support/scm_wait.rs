//! The tests' waits for a service's state, over the product's own notification wait
//! ([`goetia::backend::scm::wait::wait_until`]). Unbounded, like every wait on a state SCM must
//! reach; a state it settles in after the wait began that is not the one waited for fails the test
//! with that state instead of hanging it. After [`StopSeen::Requested`] a first read other than
//! `STOP_PENDING` or `STOPPED` keeps waiting: the stop masks deliver neither `RUNNING` nor `PAUSED`,
//! so a service that never leaves the state it was in hangs the wait.

use std::ops::ControlFlow;

use goetia::backend::scm::wait::{
    QueriedState, StopSeen, SystemScmActor, Waited, WantState, wait_after_stop, wait_until,
};
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
        waited.waited,
        Waited::Confirmed,
        "an unbounded wait for {id} to report RUNNING expired"
    );
}

/// Returns once `id` reports `STOPPED`, by the product's own stop wait; `stop` is what the caller has
/// seen of the stop. A service that is running again after it began to stop (a recovery restart) is
/// a panic naming that.
pub fn wait_for_stopped(actor: &mut SystemScmActor, id: &str, stop: StopSeen) {
    let waited = wait_after_stop(actor, Budget::Unbounded.start(), "", stop)
        .unwrap_or_else(|e| panic!("wait for {id} to reach STOPPED: {e}"));
    assert_eq!(
        waited.waited,
        Waited::Confirmed,
        "an unbounded wait for {id} to stop expired"
    );
}
