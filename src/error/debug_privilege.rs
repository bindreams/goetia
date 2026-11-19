//! A denied open that enabling `SeDebugPrivilege` for it also failed, carried through an [`io::Error`].

use std::fmt;
use std::io;

use super::composite::{Composite, carry};
use super::report::Pieces;
use super::within::carries_error;

/// An open that was denied and the step that enabling `SeDebugPrivilege` for it failed at, the second told
/// after the first: `open (enabling SeDebugPrivilege for the open also failed: step)`. Both are rendered by
/// `Display` and neither is the [`source`](std::error::Error::source), as the standard library asks. The
/// open is the cause, so its OS code is the pair's; the step is only mentioned, and never searched.
///
/// Both are raw OS errors: a failure that carries a goetia [`Error`](super::Error), directly or under a
/// context, is not one of them.
#[derive(Debug)]
struct DebugPrivilegeAlsoFailed {
    open: io::Error,
    step: io::Error,
}

impl Composite for DebugPrivilegeAlsoFailed {
    fn pieces(&self) -> Pieces<'_> {
        Pieces::new()
            .cause(&self.open)
            .text(" (enabling SeDebugPrivilege for the open also failed: ")
            .mentioned(&self.step)
            .text(")")
    }
}

impl fmt::Display for DebugPrivilegeAlsoFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.pieces().fmt(f)
    }
}

impl std::error::Error for DebugPrivilegeAlsoFailed {}

/// `open`, a denied open, with `step`, the failed step of enabling `SeDebugPrivilege` for it, told after
/// it, with `open`'s kind kept.
pub(crate) fn debug_privilege_also_failed(open: io::Error, step: io::Error) -> io::Error {
    debug_assert!(
        !carries_error(&open) && !carries_error(&step),
        "a pair is two OS failures: {open:?}, {step:?}"
    );
    carry(open.kind(), DebugPrivilegeAlsoFailed { open, step })
}

#[cfg(test)]
#[path = "debug_privilege_tests.rs"]
mod debug_privilege_tests;
