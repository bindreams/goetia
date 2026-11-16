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
pub(crate) fn carried(e: io::Error) -> Result<Error, io::Error> {
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

#[cfg(test)]
#[path = "within_tests.rs"]
mod within_tests;
