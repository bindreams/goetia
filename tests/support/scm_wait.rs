//! The tests' waits for a service's state, over the product's own notification wait
//! ([`goetia::backend::scm::wait::wait_until`]). Unbounded, like every wait on a state SCM must
//! reach; a state it settles in that is not the one waited for fails the test with that state
//! instead of hanging it.

use std::ops::ControlFlow;

use goetia::backend::scm::wait::{QueriedState, SystemScmActor, Waited, WantState, wait_after_stop, wait_until};
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

/// What the caller of [`wait_for_stopped`] has seen of the stop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StopSeen {
    /// A stop was just sent and nothing has shown yet: the first reading may still be `RUNNING`.
    Requested,
    /// `STOP_PENDING` was already seen: a first reading of `RUNNING` or `PAUSED` is a failure.
    Pending,
}

/// Returns once `id` reports `STOPPED`, by the product's own stop wait. A service that is running
/// again after it began to stop (a recovery restart) is a panic naming that.
pub fn wait_for_stopped(actor: &mut SystemScmActor, id: &str, stop: StopSeen) {
    let waited = wait_after_stop(actor, Budget::Unbounded.start(), "", stop == StopSeen::Pending)
        .unwrap_or_else(|e| panic!("wait for {id} to reach STOPPED: {e}"));
    assert_eq!(waited, Waited::Confirmed, "an unbounded wait for {id} to stop expired");
}
