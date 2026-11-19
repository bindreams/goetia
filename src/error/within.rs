use std::fmt;
use std::io;

use super::Error;
use super::composite::{self, Composite, carry};
use super::report::Pieces;

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

/// Two failures of one operation, the second told after the first: `primary (phrase second)`. Both are
/// rendered by `Display` and neither is the [`source`](std::error::Error::source), as the standard
/// library asks. The primary is the cause, so its OS code is the pair's; the second is only mentioned,
/// and never searched.
///
/// A goetia [`Error`] that the primary carries is the pair's class: [`carried`] gives it up, and the
/// second is not told after it.
#[derive(Debug)]
pub(super) struct PairedFailure {
    pub(super) primary: io::Error,
    pub(super) phrase: &'static str,
    pub(super) second: io::Error,
}

impl Composite for PairedFailure {
    fn pieces(&self) -> Pieces<'_> {
        Pieces::new()
            .cause(&self.primary)
            .text(format!(" ({} ", self.phrase))
            .mentioned(&self.second)
            .text(")")
    }

    fn into_carried(self: Box<Self>) -> Result<Error, Box<dyn Composite>> {
        let PairedFailure {
            primary,
            phrase,
            second,
        } = *self;
        carried(primary).map_err(|primary| {
            Box::new(PairedFailure {
                primary,
                phrase,
                second,
            }) as Box<dyn Composite>
        })
    }
}

impl fmt::Display for PairedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.pieces().fmt(f)
    }
}

impl std::error::Error for PairedFailure {}

/// `primary` with `second` told after it, in the words `phrase` (which ends where `second` begins),
/// with `primary`'s kind kept. [`carried`] gives up the goetia [`Error`] that `primary` carries, if it
/// carries one; any other pair is kept whole.
pub(crate) fn paired(primary: io::Error, phrase: &'static str, second: io::Error) -> io::Error {
    carry(
        primary.kind(),
        PairedFailure {
            primary,
            phrase,
            second,
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
    let e = match composite::into_carried(e) {
        Ok(error) => return Ok(error),
        Err(e) => e,
    };
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
