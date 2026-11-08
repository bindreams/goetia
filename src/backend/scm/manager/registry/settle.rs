//! What a read or a delete under `Services\<name>` makes of a key that is gone.
//!
//! goetia writes `Parameters` and `Environment` in place, so its own writes take no key away,
//! except that a write replaces a `Parameters` that is a registry link, which it meets only when
//! the link appeared between its read and its write.
//! A key still goes missing when the service is uninstalled or another tool deletes it. A service
//! that is gone is then absent; one that stands with a key that vanished is undetermined, with the
//! cause, and is not waited on: a key another process took away has no event goetia can rely on.
//! [`tolerate_not_found`] and [`gone_is_done`] say what a delete makes of a key or value gone.

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

/// NotFound is success: the goal state is absent. For a value, and for a key opened relative to a
/// held parent. KEY_DELETED is an error here: the key holding the value, or the parent, was marked
/// deleted, so what the write targets is gone and the write was lost.
///
/// For a key below the one opened, [`gone_is_done`] applies instead, which also takes a key marked
/// deleted: that key is part of what is being deleted.
pub(super) fn tolerate_not_found(deleted: io::Result<()>) -> io::Result<()> {
    match deleted {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `result`, except that a key not found or deleted is done: the goal of a delete is absent.
pub(super) fn gone_is_done(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if is_gone(&e) => Ok(()),
        other => other,
    }
}

/// Whether the service key stands, from `through`, its open as SCM reads it (a registry link is
/// followed). A key not found or deleted is gone; a denied open stands, because resolving the path
/// comes before the access check.
///
/// A key gone through the link is gone only if `itself`, asked then, finds it gone as itself too.
/// If `itself` finds a key, it is a link with a missing target: `through`'s error stands.
pub(super) fn stands(through: io::Result<()>, itself: impl FnOnce() -> io::Result<()>) -> io::Result<bool> {
    let denied = |e: &io::Error| e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    match through {
        Ok(()) => Ok(true),
        Err(e) if denied(&e) => Ok(true),
        Err(e) if is_gone(&e) => match itself() {
            Err(again) if is_gone(&again) => Ok(false),
            Err(again) if !denied(&again) => Err(again),
            Ok(()) | Err(_) => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// Resolves `stop`; `service_stands` runs for `Absent` and `Vanished` alike, since a key not found
/// says nothing about the service key above it.
pub(super) fn settle<T>(stop: Stop<T>, service_stands: impl FnOnce() -> Result<bool, Failure>) -> Settled<T> {
    let (stands, on_standing) = match stop {
        Stop::Other(failure) => return Settled::Failed(failure),
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
