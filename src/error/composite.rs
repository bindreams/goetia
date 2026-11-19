//! Errors that tell more than one failure in one message, carried through an [`io::Error`].

use std::fmt;
use std::io;

use super::report::Pieces;

/// An error that states several failures in its message, and so is not on any `source()` chain: what
/// [`Carrier`] carries. The one place a composite's shape is stated is [`pieces`](Composite::pieces), which
/// its `Display` and everything that walks it read; the walks know no composite by name.
pub(crate) trait Composite: std::error::Error + Send + Sync + 'static {
    /// What `Display` renders: its text and the errors set inside it, each a cause or only mentioned.
    fn pieces(&self) -> Pieces<'_>;
}

/// The payload of the [`io::Error`] that carries a [`Composite`], for a caller that passes failures through
/// `io::Result`. [`parts`](super::report::parts) reads it once, whatever the composite is.
#[derive(Debug)]
pub(crate) struct Carrier(Box<dyn Composite>);

impl Carrier {
    pub(super) fn composite(&self) -> &dyn Composite {
        &*self.0
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for Carrier {}

/// `composite` as an [`io::Error`] of `kind`.
pub(crate) fn carry(kind: io::ErrorKind, composite: impl Composite) -> io::Error {
    io::Error::new(kind, Carrier(Box::new(composite)))
}

/// The `T` that `e` carries as a composite, if it does.
pub(crate) fn carried_composite<T: Composite>(e: &io::Error) -> Option<&T> {
    let carrier = e.get_ref()?.downcast_ref::<Carrier>()?;
    (carrier.composite() as &(dyn std::error::Error + 'static)).downcast_ref::<T>()
}
