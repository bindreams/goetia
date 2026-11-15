//! An error that arrives with another one found on the way to it.

use std::io;

use crate::error::Chain;

/// A failure that came with a second one the caller must see. Printed as `<primary> (<note>:
/// <secondary>)`, each as [`Chain`] writes it. Neither is the [`source`](std::error::Error::source),
/// which would print it a second time; [`primary`](Self::primary) is how [`Error::raw_os_error`] finds the
/// primary's OS code, which the type would otherwise hide.
///
/// [`Error::raw_os_error`]: crate::error::Error::raw_os_error
#[derive(Debug, thiserror::Error)]
#[error("{} ({note}: {})", Chain(primary), Chain(secondary))]
pub(crate) struct ErrorWithNote {
    primary: io::Error,
    note: &'static str,
    secondary: io::Error,
}

impl ErrorWithNote {
    /// `primary`'s error, of its kind, carrying `secondary` after `note`.
    pub(crate) fn into_io(primary: io::Error, note: &'static str, secondary: io::Error) -> io::Error {
        io::Error::new(
            primary.kind(),
            Self {
                primary,
                note,
                secondary,
            },
        )
    }

    pub(crate) fn primary(&self) -> &io::Error {
        &self.primary
    }
}
