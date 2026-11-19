//! Two OS failures of one operation, carried through an [`io::Error`].

use std::fmt;
use std::io;

use super::composite::{Composite, carry};
use super::report::Pieces;

/// An open that was denied and the step that enabling `SeDebugPrivilege` for it failed at, the second told
/// after the first: `open (enabling SeDebugPrivilege for the open also failed: step)`. Both are rendered by
/// `Display` and neither is the [`source`](std::error::Error::source), as the standard library asks. The
/// open is the cause, so its OS code is the pair's; the step is only mentioned, and never searched.
///
/// Both are raw OS errors: a failure that carries a goetia [`Error`](super::Error) is not a pair.
#[derive(Debug)]
struct PairedFailure {
    open: io::Error,
    step: io::Error,
}

impl Composite for PairedFailure {
    fn pieces(&self) -> Pieces<'_> {
        Pieces::new()
            .cause(&self.open)
            .text(" (enabling SeDebugPrivilege for the open also failed: ")
            .mentioned(&self.step)
            .text(")")
    }
}

impl fmt::Display for PairedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.pieces().fmt(f)
    }
}

impl std::error::Error for PairedFailure {}

/// `open` with `step` told after it, with `open`'s kind kept.
pub(crate) fn paired(open: io::Error, step: io::Error) -> io::Error {
    debug_assert!(
        !carries_goetia_error(&open) && !carries_goetia_error(&step),
        "a pair is two OS failures: {open:?}, {step:?}"
    );
    carry(open.kind(), PairedFailure { open, step })
}

fn carries_goetia_error(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<super::Error>())
}

#[cfg(test)]
#[path = "paired_tests.rs"]
mod paired_tests;
