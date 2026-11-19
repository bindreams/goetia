use std::io;

use super::Error;
use super::report::{Seen, seen};

#[derive(Debug, thiserror::Error)]
#[error("{context}")]
struct Within {
    context: String,
    /// The context says that the signal of the operation was delivered: see [`within_delivered`].
    delivered: bool,
    #[source]
    source: io::Error,
}

/// `source` said in `context`, with its kind kept. The failure under `context` is the error's
/// [`source`](std::error::Error::source), which [`Error::report`](crate::Error::report) prints
/// after it: `context: source`.
pub(crate) fn within(context: impl Into<String>, source: io::Error) -> io::Error {
    said_within(context.into(), false, source)
}

/// [`within`], for a `context` that says the signal of the operation was delivered. A [`target_failed`]
/// operation is then not called a failure: the signal stands, and the context says what failed after it.
/// Only a delivered signal is told so; every other `within` is a failure of the operation.
pub(crate) fn within_delivered(context: impl Into<String>, source: io::Error) -> io::Error {
    said_within(context.into(), true, source)
}

fn said_within(context: String, delivered: bool, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        Within {
            context,
            delivered,
            source,
        },
    )
}

/// What an [`io::Error`] is, one layer down, as [`seen`] looks through what carries it: the one place that
/// knows how [`within`] is laid in it.
enum Layer<'a> {
    Context(&'a Within),
    Carried,
    Os,
}

fn layer(e: &io::Error) -> Layer<'_> {
    match seen(e) {
        Seen::Ours(_) => Layer::Carried,
        Seen::Other(inner) => inner.downcast_ref::<Within>().map_or(Layer::Os, Layer::Context),
        Seen::Composite(_) => Layer::Os,
    }
}

/// The `T` that `e` holds directly as its payload, moved out; `e` itself when it holds another, or a `T`
/// under a shared payload, which cannot be moved out of it.
fn take_payload<T: std::error::Error + 'static>(e: io::Error) -> Result<T, io::Error> {
    if !e.get_ref().is_some_and(|payload| payload.is::<T>()) {
        return Err(e);
    }
    let payload = e.into_inner().expect("a payload was just seen");
    Ok(*payload.downcast::<T>().expect("a `T` was just seen"))
}

/// Whether `e` carries a goetia [`Error`], directly or under [`within`] contexts: the question
/// [`target_failed`] and `debug_privilege_also_failed` (Windows) ask.
pub(super) fn carries_error(e: &io::Error) -> bool {
    match layer(e) {
        Layer::Context(within) => carries_error(&within.source),
        Layer::Carried => true,
        Layer::Os => false,
    }
}

/// Whether `e` says, directly or under other [`within`] contexts, that the signal of its operation was
/// delivered: see [`within_delivered`].
fn delivered(e: &io::Error) -> bool {
    match layer(e) {
        Layer::Context(within) => within.delivered || delivered(&within.source),
        Layer::Carried | Layer::Os => false,
    }
}

/// `e` as an [`Error`]: the goetia error it carries, else its failure alone as [`Error::RawOs`]. A
/// context it is said within is a [`Error::Step`] over what is under it. What a shared payload holds
/// cannot be moved out of it: it is `e` alone as [`Error::RawOs`], which every walk looks through.
fn told(e: io::Error) -> Error {
    match layer(&e) {
        Layer::Os => Error::RawOs { os: e },
        Layer::Carried => match take_payload::<Error>(e) {
            Ok(carried) => carried,
            Err(e) => match take_payload::<Box<Error>>(e) {
                Ok(carried) => *carried,
                Err(e) => Error::RawOs { os: e },
            },
        },
        Layer::Context(_) => match take_payload::<Within>(e) {
            Ok(Within { context, source, .. }) => Error::Step {
                what: context,
                source: Box::new(told(source)),
            },
            Err(e) => Error::RawOs { os: e },
        },
    }
}

/// The failure `e` of a target (a manager, a process), which reports through [`io::Error`], as an
/// [`Error`], told under `what`, the operation:
///
/// - an error said within a context by [`within_delivered`], at any depth of contexts, says that the
///   operation's signal was delivered and what failed after it, so `what` names the operation with no
///   verdict on it;
/// - any other failure is one of `what`: `failed to <what>`, over the goetia error it carries, else an OS
///   failure.
pub(crate) fn target_failed(what: impl Into<String>, e: io::Error) -> Error {
    let what = what.into();
    if delivered(&e) {
        Error::Step {
            what,
            source: Box::new(told(e)),
        }
    } else if carries_error(&e) {
        Error::Step {
            what: format!("failed to {what}"),
            source: Box::new(told(e)),
        }
    } else {
        Error::os(what, e)
    }
}

/// The failure of a hold of the process `pid`, as a target reports it through [`io::Error`] with the
/// kind of `read`: the open of the process was refused with `open_refused`, and the re-read of what runs
/// the daemon, which would say whether the refusal was the daemon's, failed with `read`. The error's
/// source is the goetia error that `read` carries, else its failure alone.
pub(crate) fn unread_after_open_refused(pid: u32, open_refused: io::Error, read: io::Error) -> io::Error {
    let kind = read.kind();
    io::Error::new(
        kind,
        Error::ManagerUnreadAfterOpenRefused {
            pid,
            open_refused,
            source: Box::new(told(read)),
        },
    )
}

#[cfg(test)]
#[path = "within_tests.rs"]
mod within_tests;
