use std::io;

use super::Error;

#[derive(Debug, thiserror::Error)]
#[error("{context}")]
struct Within {
    context: String,
    #[source]
    source: io::Error,
}

/// `source` said in `context`, with its kind kept. The failure under `context` is the error's
/// [`source`](std::error::Error::source), which [`Error::report`](crate::Error::report) prints
/// after it: `context: source`.
pub(crate) fn within(context: impl Into<String>, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        Within {
            context: context.into(),
            source,
        },
    )
}

/// The goetia [`Error`] that `e` carries, directly or under [`within`] contexts.
fn carried_error(e: &io::Error) -> Option<&Error> {
    let inner = e.get_ref()?;
    match inner.downcast_ref::<Within>() {
        Some(within) => carried_error(&within.source),
        None => inner.downcast_ref::<Error>(),
    }
}

/// Whether `e` carries a goetia [`Error`], as [`target_failed`] and [`unread_after_open_refused`] look
/// for one.
pub(super) fn carries_error(e: &io::Error) -> bool {
    carried_error(e).is_some()
}

/// `e` as an [`Error`]: the goetia error it carries, else its failure alone as [`Error::RawOs`]. A
/// context it is said within is a [`Error::Step`] over what is under it.
fn told(e: io::Error) -> Error {
    let Some(inner) = e.get_ref() else {
        return Error::RawOs { os: e };
    };
    if inner.is::<Error>() {
        let inner = e.into_inner().expect("an error with a source was just seen");
        return *inner.downcast::<Error>().expect("an `Error` was just seen");
    }
    if inner.is::<Within>() {
        let inner = e.into_inner().expect("an error with a source was just seen");
        let Within { context, source } = *inner.downcast::<Within>().expect("a `Within` was just seen");
        return Error::Step {
            what: context,
            source: Box::new(told(source)),
        };
    }
    Error::RawOs { os: e }
}

/// The failure `e` of a target (a manager, a process), which reports through [`io::Error`], as an
/// [`Error`], told under `what`, the operation:
///
/// - an error said within a context (see [`within`]) says how far the operation got, so `what` names the
///   operation with no verdict on it: the context may say that the signal of a kill was delivered;
/// - any other failure is one of `what`: `failed to <what>`, over the goetia error it carries, else an OS
///   failure.
pub(crate) fn target_failed(what: impl Into<String>, e: io::Error) -> Error {
    let what = what.into();
    if e.get_ref().is_some_and(|inner| inner.is::<Within>()) {
        return Error::Step {
            what,
            source: Box::new(told(e)),
        };
    }
    if carries_error(&e) {
        return Error::Step {
            what: format!("failed to {what}"),
            source: Box::new(told(e)),
        };
    }
    Error::os(what, e)
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
