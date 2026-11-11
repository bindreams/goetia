//! A failed spawn's child, given up instead of waited for.

use cosca::error::Error;

/// `e`, with the child cosca could not end given up. Dropping an [`Unreaped`](cosca::Unreaped)
/// waits with no bound for the child to exit, and a spawn error is dropped where it is reported, so
/// a child whose kill was refused would stall a call past its deadline. The child is left running,
/// as before cosca handed it back; its [`fate`](Error::fate) still says so.
pub(crate) fn release_unreaped(e: Error) -> Error {
    match e {
        Error::MayHaveStarted {
            fate,
            wrapper_elevated,
            source,
            unreaped: Some(child),
        } => {
            child.leak();
            Error::MayHaveStarted {
                fate,
                wrapper_elevated,
                source,
                unreaped: None,
            }
        }
        other => other,
    }
}

#[cfg(test)]
#[path = "unreaped_tests.rs"]
mod unreaped_tests;
