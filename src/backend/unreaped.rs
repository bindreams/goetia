//! A failed spawn's child, settled where the failure is reported.

use cosca::error::Error;

#[cfg(unix)]
use crate::backend::standby::Standby;

/// `e`, with the child cosca hands back (one it could not show collected) settled so that the caller
/// never waits for it. Dropping an [`Unreaped`](cosca::Unreaped) waits with no bound for the child
/// to exit, and a spawn error is dropped where it is reported.
///
/// On Unix the child is waited for on a thread of its own, so it is reaped once it exits. With no
/// thread to be had it is given up unreaped: whenever it exits before this process, it stays a zombie
/// until this process exits. On Windows it is given up, which only closes a handle. What the child
/// may be doing is its [`fate`](Error::fate), which is cosca's.
pub(crate) fn release_unreaped(mut e: Error) -> Error {
    if let Error::MayHaveStarted { unreaped, .. } = &mut e
        && let Some(child) = unreaped.take()
    {
        settle(*child);
    }
    e
}

#[cfg(unix)]
fn settle(child: cosca::Unreaped) {
    // The status is dropped: the caller has reported the failure, and the child is settled whatever
    // the wait said.
    if let Err(child) = Standby::run_now("goetia-reaper", child, |child| drop(child.wait())) {
        child.leak();
    }
}

#[cfg(not(unix))]
fn settle(child: cosca::Unreaped) {
    child.leak();
}

#[cfg(test)]
#[path = "unreaped_tests.rs"]
mod unreaped_tests;
