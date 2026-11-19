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

/// Two failures of one operation, the second told after the first: `primary (words second)`. Both are
/// rendered by `Display` and neither is the [`source`](std::error::Error::source), as the standard
/// library asks. The primary is the cause, so its OS code is the pair's; the second is only mentioned,
/// and never searched.
///
/// When the primary carries a goetia [`Error`], [`carried`] gives the pair as the [`Error`] its
/// [`PairKind`] names, with that error as its source: its class is the pair's, and the second is told.
#[derive(Debug)]
pub(super) struct PairedFailure {
    pub(super) primary: io::Error,
    pub(super) kind: PairKind,
    pub(super) second: io::Error,
}

/// What two failures that [`paired`] tells are: the words between them, and the [`Error`] they become
/// when the first carries one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairKind {
    /// The manager could not be read to tell whether the open that was refused is the service's: the
    /// first is the read, the second the open.
    ManagerUnreadAfterOpenRefused,
    /// An open was denied, and enabling `SeDebugPrivilege` for it failed too: the first is the open, the
    /// second the step. Two OS failures, so no goetia error is carried to classify them.
    #[cfg(windows)]
    DebugPrivilegeAlsoFailed,
}

/// The [`Error`] a pair becomes around the goetia error its first failure carried, and its second.
type PairInto = fn(Error, io::Error) -> Error;

impl PairKind {
    /// The words that end where the second begins, and the [`Error`] the pair becomes, if it can.
    fn shape(self) -> (&'static str, Option<PairInto>) {
        match self {
            PairKind::ManagerUnreadAfterOpenRefused => (
                "after the service's process could not be opened:",
                Some(|read, open| Error::ManagerUnreadAfterOpenRefused {
                    open_refused: open,
                    source: Box::new(read),
                }),
            ),
            #[cfg(windows)]
            PairKind::DebugPrivilegeAlsoFailed => ("enabling SeDebugPrivilege for the open also failed:", None),
        }
    }
}

impl Composite for PairedFailure {
    fn pieces(&self) -> Pieces<'_> {
        Pieces::new()
            .cause(&self.primary)
            .text(format!(" ({} ", self.kind.shape().0))
            .mentioned(&self.second)
            .text(")")
    }

    fn into_carried(self: Box<Self>) -> Result<Error, Box<dyn Composite>> {
        let PairedFailure { primary, kind, second } = *self;
        let Some(into_error) = kind.shape().1 else {
            return Err(Box::new(PairedFailure { primary, kind, second }));
        };
        match carried(primary) {
            Ok(error) => Ok(into_error(error, second)),
            Err(primary) => Err(Box::new(PairedFailure { primary, kind, second })),
        }
    }
}

impl fmt::Display for PairedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.pieces().fmt(f)
    }
}

impl std::error::Error for PairedFailure {}

/// `primary` with `second` told after it, in the words of `kind`, with `primary`'s kind kept.
/// [`carried`] gives a pair whose primary carries a goetia [`Error`] as the [`Error`] `kind` names.
pub(crate) fn paired(primary: io::Error, kind: PairKind, second: io::Error) -> io::Error {
    carry(primary.kind(), PairedFailure { primary, kind, second })
}

/// The goetia [`Error`] that `e` carries, for a caller that passed one through an [`io::Error`], or
/// else `e`. One carried under [`within`] keeps its variant, as the source of an [`Error::Step`] that
/// says the context.
fn carried(e: io::Error) -> Result<Error, io::Error> {
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

/// The failure `e` of a target (a manager, a process), which reports through [`io::Error`], as an
/// [`Error`]: the goetia error it [`carried`], or else an OS failure of `what`.
pub(crate) fn target_failed(what: impl Into<String>, e: io::Error) -> Error {
    carried(e).unwrap_or_else(|e| Error::os(what, e))
}

#[cfg(test)]
#[path = "within_tests.rs"]
mod within_tests;
