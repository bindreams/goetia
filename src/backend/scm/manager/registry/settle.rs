//! What a read under `Services\<name>` makes of a key that is gone.
//!
//! goetia writes `Parameters` and `Environment` in place, so its own writes never take a key away.
//! A key still goes missing when the service is uninstalled, or another tool deletes it, and a read
//! that meets that must not guess: a service that is gone is absent, and one that stands is
//! neither absent nor foreign on the strength of a read that did not complete.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_KEY_DELETED};

/// A registry failure as facts: the operation, the key and the OS error.
#[derive(Debug)]
pub(super) struct Failure {
    pub op: String,
    pub path: String,
    pub source: io::Error,
}

impl Failure {
    fn is_deleted(&self) -> bool {
        self.source.raw_os_error() == Some(ERROR_KEY_DELETED as i32)
    }

    fn is_not_found(&self) -> bool {
        self.source.kind() == io::ErrorKind::NotFound
    }
}

/// Why one pass of a read did not complete.
#[derive(Debug)]
pub(super) enum Stop<T> {
    /// The key a service may lack was not found at open. `T` is what that reads as for a service
    /// that stands without it.
    Absent(T),
    /// A key was not found at open or deleted after it, where a service that stands has it.
    Vanished,
    /// `ERROR_KEY_DELETED` at open.
    DeletedAtOpen(Failure),
    /// Any other failure.
    Other(Failure),
}

impl<T> Stop<T> {
    /// `failure` of an open. `absent` is `Some` for a key a service may lack.
    pub fn at_open(failure: Failure, absent: Option<T>) -> Self {
        if failure.is_not_found() {
            match absent {
                Some(value) => Stop::Absent(value),
                None => Stop::Vanished,
            }
        } else if failure.is_deleted() {
            Stop::DeletedAtOpen(failure)
        } else {
            Stop::Other(failure)
        }
    }

    /// `failure` of a read of a key already open.
    pub fn mid_read(failure: Failure) -> Self {
        if failure.is_deleted() || failure.is_not_found() {
            Stop::Vanished
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
    /// The service stands and a key was swapped under the read: read again from the start.
    Retry,
    Failed(Failure),
}

/// Whether the service key opened. A key not found or deleted is gone; a denied open stands,
/// because resolving the path comes before the access check.
pub(super) fn stands(opened: io::Result<()>) -> io::Result<bool> {
    match opened {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ERROR_KEY_DELETED as i32) => {
            Ok(false)
        }
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => Ok(true),
        Err(e) => Err(e),
    }
}

/// `stop`, given `service_stands`, which is asked only when the answer depends on it.
///
/// A `DeletedAtOpen` on a service that stands is not retried: a fresh open meets the same key.
pub(super) fn settle<T>(stop: Stop<T>, service_stands: impl FnOnce() -> Result<bool, Failure>) -> Settled<T> {
    let (stands, on_standing) = match stop {
        Stop::Other(failure) => return Settled::Failed(failure),
        Stop::Absent(value) => (service_stands(), Settled::Value(value)),
        Stop::Vanished => (service_stands(), Settled::Retry),
        Stop::DeletedAtOpen(failure) => (service_stands(), Settled::Failed(failure)),
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
