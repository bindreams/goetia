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

/// The goetia [`Error`] that `e` carries, for a caller that passed one through an [`io::Error`], or
/// else `e`. One carried under [`within`] keeps its variant, as the source of an [`Error::Step`] that
/// says the context.
fn carried(e: io::Error) -> Result<Error, io::Error> {
    if e.get_ref().is_some_and(|inner| inner.is::<Error>()) {
        let inner = e.into_inner().expect("an error with a source was just seen");
        return Ok(*inner.downcast::<Error>().expect("an `Error` was just seen"));
    }
    if !e.get_ref().is_some_and(|inner| inner.is::<Within>()) {
        return Err(e);
    }
    let inner = e.into_inner().expect("an error with a source was just seen");
    let Within { context, source } = *inner.downcast::<Within>().expect("a `Within` was just seen");
    match carried(source) {
        Ok(error) => Ok(Error::Step {
            what: context,
            source: Box::new(error),
        }),
        Err(source) => Err(within(context, source)),
    }
}

/// The failure `e` of a target (a manager, a process), which reports through [`io::Error`], as an
/// [`Error`]: the goetia error it [`carried`], under a step that says `what` failed, or else an OS failure
/// of `what`.
pub(crate) fn target_failed(what: impl Into<String>, e: io::Error) -> Error {
    let what = what.into();
    match carried(e) {
        Ok(error) => Error::Step {
            what: format!("failed to {what}"),
            source: Box::new(error),
        },
        Err(e) => Error::os(what, e),
    }
}

/// The failure of a hold of the process `pid`, as a target reports it through [`io::Error`] with the
/// kind of `read`: the open of the process was refused with `open_refused`, and the re-read of what runs
/// the daemon, which would say whether the refusal was the daemon's, failed with `read`. A goetia error
/// that `read` carries is the error's source, else an OS failure of the read.
pub(crate) fn unread_after_open_refused(pid: u32, open_refused: io::Error, read: io::Error) -> io::Error {
    let kind = read.kind();
    let source = carried(read).unwrap_or_else(|read| Error::os("read it", read));
    io::Error::new(
        kind,
        Error::ManagerUnreadAfterOpenRefused {
            pid,
            open_refused,
            source: Box::new(source),
        },
    )
}

#[cfg(test)]
#[path = "within_tests.rs"]
mod within_tests;
