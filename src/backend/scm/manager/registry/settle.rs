//! What a read under `Services\<name>` makes of a key that is gone.
//!
//! goetia writes `Parameters` and `Environment` in place, so its own writes take no key away, but
//! for a `Parameters` that is a registry link, which `install --force` replaces. A key still goes
//! missing when the service is uninstalled or another tool deletes it. A service
//! that is gone is then absent; one that stands with a key that vanished is undetermined, with the
//! cause, and is not waited on: a key another process took away has no event goetia can rely on.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_KEY_DELETED};

#[derive(Debug)]
pub(super) struct Failure {
    pub op: String,
    pub path: String,
    pub source: io::Error,
}

impl Failure {
    fn is_gone(&self) -> bool {
        is_gone(&self.source)
    }
}

/// Why one pass of a read did not complete.
#[derive(Debug)]
pub(super) enum Stop<T> {
    /// The key a service may lack was not found at open, which does not say whether the service
    /// key above it is there. `T` is what that reads as for a service that stands without it.
    Absent(T),
    /// A key was not found or marked deleted, where a service that stands has it. The failure is
    /// what the read reports if the service stands.
    Vanished(Failure),
    /// Any other failure.
    Other(Failure),
}

impl<T> Stop<T> {
    /// `failure` of an open. `absent` is `Some` for a key a service may lack.
    pub fn at_open(failure: Failure, absent: Option<T>) -> Self {
        if failure.source.kind() == io::ErrorKind::NotFound
            && let Some(value) = absent
        {
            Stop::Absent(value)
        } else if failure.is_gone() {
            Stop::Vanished(failure)
        } else {
            Stop::Other(failure)
        }
    }

    /// `failure` of a read of a key already open.
    pub fn mid_read(failure: Failure) -> Self {
        if failure.is_gone() {
            Stop::Vanished(failure)
        } else {
            Stop::Other(failure)
        }
    }
}

/// What a read does after a [`Stop`].
#[derive(Debug)]
pub(super) enum Settled<T> {
    /// The service no longer exists.
    Gone,
    /// The service stands and has no such key: the read's value.
    Value(T),
    /// The read did not complete: another failure, a failed existence check, or a key that vanished
    /// under a service that stands.
    Failed(Failure),
}

/// Whether `e` says the key is not there: not found, or deleted and held open by another process.
pub(super) fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ERROR_KEY_DELETED as i32)
}

/// `result`, except that a key not found or deleted is done: the goal of a delete is absent.
pub(super) fn gone_is_done(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if is_gone(&e) => Ok(()),
        other => other,
    }
}

/// Whether the service key opened. A key not found or deleted is gone; a denied open stands,
/// because resolving the path comes before the access check.
pub(super) fn stands(opened: io::Result<()>) -> io::Result<bool> {
    match opened {
        Ok(()) => Ok(true),
        Err(e) if is_gone(&e) => Ok(false),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => Ok(true),
        Err(e) => Err(e),
    }
}

/// What vouches that the service stands, for a read that finds an optional key missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// No enumeration vouches for the name: the service was looked up by name, so a missing
    /// optional key may mean it is gone, and the read asks.
    Opened,
    /// Something other than a lookup vouches for the name: the enumeration of `Services` listed it,
    /// or the caller holds an open handle to the service. A missing optional key, or a service key
    /// that is gone, is then taken as the service standing without it: a service that is gone
    /// leaves at most a stale row, which the user's own concurrent uninstall explains, and a
    /// deleted service that is held is only marked, its key staying until the handle closes.
    Vouched,
}

/// Resolves `stop`; `service_stands` runs for `Vanished`, and for `Absent` unless the name is
/// [`Origin::Vouched`].
pub(super) fn settle<T>(
    stop: Stop<T>,
    origin: Origin,
    service_stands: impl FnOnce() -> Result<bool, Failure>,
) -> Settled<T> {
    let (stands, on_standing) = match stop {
        Stop::Other(failure) => return Settled::Failed(failure),
        Stop::Absent(value) if origin == Origin::Vouched => return Settled::Value(value),
        Stop::Absent(value) => (service_stands(), Settled::Value(value)),
        Stop::Vanished(failure) => (service_stands(), Settled::Failed(failure)),
    };
    match stands {
        Ok(true) => on_standing,
        Ok(false) => Settled::Gone,
        Err(failure) => Settled::Failed(failure),
    }
}

#[cfg(test)]
#[path = "settle_tests.rs"]
mod settle_tests;
