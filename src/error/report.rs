//! Printing an [`Error`]: its chain of causes, then each remedy found along it.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::sync::Arc;

use super::composite::{Carrier, Composite};
use super::escape::{write_escaped, write_escaped_keeping_layout};
use super::{Error, SharedError};

/// What [`Error::report`] and `Error::report_unescaped` return: the chain, formatted on `Display`.
pub(super) struct Report<'a> {
    pub(super) error: &'a Error,
    pub(super) escaped: bool,
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_report(self.error, &mut Sink::new(f, self.escaped))
    }
}

/// `e` as [`Error::report`] prints it: its chain, then each remedy found along it.
pub(super) fn write_report(e: &Error, out: &mut Sink) -> fmt::Result {
    let mut remedies = Vec::new();
    write_chain(e, out, &mut remedies)?;
    write_remedies(out, &remedies)
}

/// Where error text goes. The one place it is written for a person is [`write_chain`], so the one
/// place it is escaped: `escaped` is whether this destination is one. The text goes through
/// [`fmt::Write`]; a child's stderr is written with [`write_layout`](Sink::write_layout). A sink
/// writes a child's stderr only if it was made to: [`Chain`] makes one that does not. It remembers
/// the last character written, so that [`write_remedy`](Sink::write_remedy) can join a remedy to it.
pub(crate) struct Sink<'a> {
    out: &'a mut dyn fmt::Write,
    escaped: bool,
    stderr: bool,
    last: Option<char>,
}

impl<'a> Sink<'a> {
    pub(crate) fn new(out: &'a mut dyn fmt::Write, escaped: bool) -> Self {
        Sink {
            out,
            escaped,
            stderr: true,
            last: None,
        }
    }

    /// A sink that writes text as it is and leaves out a child's stderr.
    fn without_stderr(out: &'a mut dyn fmt::Write) -> Self {
        Sink {
            out,
            escaped: false,
            stderr: false,
            last: None,
        }
    }

    /// Writes `text`, a child's diagnostic, as it is laid out: for a person its line feeds, tabs
    /// and CRLFs are kept and the rest of the escaped set is escaped.
    fn write_layout(&mut self, text: &str) -> fmt::Result {
        self.last = text.chars().next_back().or(self.last);
        if self.escaped {
            write_escaped_keeping_layout(self.out, text)
        } else {
            self.out.write_str(text)
        }
    }
}

impl Sink<'_> {
    /// Writes `remedy` as a sentence after what is written so far: `. remedy`, or ` remedy` when
    /// that already ends in `.`, `!` or `?`. A remedy needs words before it.
    pub(crate) fn write_remedy(&mut self, remedy: &str) -> fmt::Result {
        debug_assert!(self.last.is_some(), "a remedy follows words");
        let join = if matches!(self.last, Some('.' | '!' | '?')) {
            " "
        } else {
            ". "
        };
        self.write_str(join)?;
        self.write_str(remedy)
    }
}

impl fmt::Write for Sink<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.last = text.chars().next_back().or(self.last);
        if self.escaped {
            write_escaped(self.out, text)
        } else {
            self.out.write_str(text)
        }
    }
}

/// An error and its causes, without the remedies [`Report`] prints after them and without a failed
/// command's stderr, as [`Error::Command`]'s own `Display` leaves it out: how an error that sits
/// inside another's message is written to `Display`.
///
/// Not escaped: error reports escape at the sink; log lines and stderr warnings print it raw.
pub(crate) struct Chain<'a>(pub(crate) &'a (dyn std::error::Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_chain(self.0, &mut Sink::without_stderr(f), &mut Vec::new())
    }
}

type Dyn = dyn std::error::Error + 'static;

// The shape of an error ===============================================================================================

/// One piece of a message that has errors set inside it.
pub(crate) enum Piece<'a> {
    Text(Cow<'a, str>),
    /// An error the message renders. `cause` is whether it is a cause of the failure the message states,
    /// so that its OS code is the failure's ([`find`]); it is not for an error mentioned in passing, as
    /// the failed attempt to end a process under a timeout that is no OS failure.
    Error {
        error: &'a Dyn,
        cause: bool,
    },
}

/// A message with errors set inside it: text and errors, in the order they are said. The one thing
/// that renders such a message, whether it is an error's `Display` or a [`Reason`](super::Reason):
/// the errors are written as any error is, so their remedies are collected.
pub(crate) struct Pieces<'a>(Vec<Piece<'a>>);

impl<'a> Pieces<'a> {
    pub(crate) fn new() -> Self {
        Pieces(Vec::new())
    }

    pub(crate) fn text(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.0.push(Piece::Text(text.into()));
        self
    }

    /// `error`, a cause of the failure this message states.
    pub(crate) fn cause(mut self, error: &'a Dyn) -> Self {
        self.0.push(Piece::Error { error, cause: true });
        self
    }

    /// `error`, which the message mentions and is not a cause of what it states.
    pub(crate) fn mentioned(mut self, error: &'a Dyn) -> Self {
        self.0.push(Piece::Error { error, cause: false });
        self
    }

    /// All of `other`, after what is here.
    pub(crate) fn then(mut self, other: Pieces<'a>) -> Self {
        self.0.extend(other.0);
        self
    }

    /// The errors set inside the message, each with whether it is a cause.
    pub(crate) fn errors(&self) -> impl Iterator<Item = (&'a Dyn, bool)> + '_ {
        self.0.iter().filter_map(|piece| match piece {
            Piece::Error { error, cause } => Some((*error, *cause)),
            Piece::Text(_) => None,
        })
    }

    /// Writes the message into `out`, collecting into `remedies` those of the errors in it.
    pub(super) fn write(&self, out: &mut Sink, remedies: &mut Vec<&'a str>) -> fmt::Result {
        for piece in &self.0 {
            match piece {
                Piece::Text(text) => out.write_str(text)?,
                Piece::Error { error, .. } => write_chain(*error, out, remedies)?,
            }
        }
        Ok(())
    }
}

/// As [`Chain`] writes each error in it: no remedies, no stderr.
impl fmt::Display for Pieces<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(&mut Sink::without_stderr(f), &mut Vec::new())
    }
}

/// How an error renders itself.
pub(super) enum Shape<'a> {
    /// Its own `Display`.
    Own,
    /// Through [`cosca::error::Report`]: a `cosca` list's members and its cleanup are not on `source()`.
    Cosca,
    /// Text with errors set inside it. What `Display` renders is exactly this, and none of the errors in
    /// it is also the [`source`](Node::source): an error is rendered by its `Display` or returned by
    /// `source()`, never both, as the standard library asks.
    Pieces(Pieces<'a>),
}

/// An error, as everything that walks one sees it: how it renders and what it returns from `source()`.
pub(super) struct Node<'a> {
    /// The error itself, as it is after [`parts`] has looked through what carries it.
    pub(super) error: &'a Dyn,
    /// The goetia error this is, which knows its remedy and its stderr.
    pub(super) ours: Option<&'a Error>,
    pub(super) shape: Shape<'a>,
    pub(super) source: Option<&'a Dyn>,
}

/// What an error is once everything that only carries another has been looked through: the one
/// description of how an [`io::Error`](std::io::Error) and the shared and boxed errors nest one, which
/// [`parts`] and every other walk of an `io::Error` read.
///
/// What is looked through: an `io::Error`'s custom payload (its `source()` is the payload's, which skips
/// the payload), an `Arc<io::Error>` or [`SharedError`], and a `Box<Error>`. A [`Carrier`] is its
/// composite. A goetia [`Error`] is itself, a [`Kept`](Error::Kept) one as the error it holds.
pub(super) enum Seen<'a> {
    Ours(&'a Error),
    Composite(&'a dyn Composite),
    Other(&'a Dyn),
}

pub(super) fn seen<'a>(mut e: &'a Dyn) -> Seen<'a> {
    loop {
        // An `Arc`'s `source()` is its content's, which skips the content: a shared error is looked
        // through for the error it holds. `Arc<io::Error>` is how `cosca` shares one; `SharedError` is what
        // `Blob` and `Invalid` keep. A `#[source]` of type `Box<Error>` reaches here as the box.
        let inner: &'a Dyn = if let Some(payload) = e.downcast_ref::<std::io::Error>().and_then(std::io::Error::get_ref)
        {
            payload
        } else if let Some(carrier) = e.downcast_ref::<Carrier>() {
            return Seen::Composite(carrier.composite());
        } else if let Some(shared) = e.downcast_ref::<Arc<std::io::Error>>() {
            &**shared
        } else if let Some(shared) = e.downcast_ref::<SharedError>() {
            &**shared
        } else if let Some(ours) = e
            .downcast_ref::<Error>()
            .or_else(|| e.downcast_ref::<Box<Error>>().map(|b| &**b))
        {
            return Seen::Ours(ours.unkept());
        } else {
            return Seen::Other(e);
        };
        e = inner;
    }
}

/// `e`'s [`Node`]: the one description of the shape of any error, which the printer and the search
/// both read, over what [`seen`] says it is.
///
/// A `cosca` error is [`Shape::Cosca`]. A goetia error that sets another inside its message, and any
/// [`Composite`](super::Composite) in its [`Carrier`], are [`Shape::Pieces`]. Any other is its own
/// `Display` and `source()`.
pub(super) fn parts<'a>(e: &'a Dyn) -> Node<'a> {
    match seen(e) {
        Seen::Composite(composite) => Node {
            error: composite,
            ours: None,
            shape: Shape::Pieces(composite.pieces()),
            source: composite.source(),
        },
        Seen::Ours(ours) => {
            let error: &Dyn = ours;
            Node {
                error,
                ours: Some(ours),
                shape: ours.pieces().map_or(Shape::Own, Shape::Pieces),
                source: error.source(),
            }
        }
        Seen::Other(error) => Node {
            error,
            ours: None,
            shape: if error.is::<cosca::error::Error>() {
                Shape::Cosca
            } else {
                Shape::Own
            },
            source: error.source(),
        },
    }
}

// Printing ============================================================================================================

/// Writes `e` and its causes, collecting the remedies met on the way into `remedies`, outermost
/// first and each once: a [`Both`](Error::Both)'s halves can carry the same one. What is written is
/// what [`parts`] says: the error's message (with the errors set inside it written as `e` is), its
/// child's stderr where the sink takes it, then its `source()`.
///
/// A `cosca` error and everything below it is written by [`cosca::error::Report`], since a `source()`
/// walk shows only the first member of a list and never a `cleanup`.
pub(super) fn write_chain<'a>(e: &'a Dyn, out: &mut Sink, remedies: &mut Vec<&'a str>) -> fmt::Result {
    let node = parts(e);
    if let Some(remedy) = node.ours.and_then(Error::remedy)
        && !remedies.contains(&remedy)
    {
        remedies.push(remedy);
    }
    match &node.shape {
        Shape::Cosca => return write!(out, "{}", cosca::error::Report::new(node.error)),
        Shape::Own => write!(out, "{}", node.error)?,
        Shape::Pieces(pieces) => pieces.write(out, remedies)?,
    }
    if out.stderr
        && let Some(stderr) = node.ours.and_then(Error::stderr).filter(|stderr| !stderr.is_empty())
    {
        out.write_str(": ")?;
        out.write_layout(stderr)?;
    }
    if let Some(source) = node.source {
        out.write_str(": ")?;
        write_chain(source, out, remedies)?;
    }
    Ok(())
}

// Searching ===========================================================================================================

/// The first thing `found` finds in `e`, searching depth first: the error itself, then its `source()`
/// chain, then the errors its message sets inside it as causes ([`Piece::Error`]), each searched the same
/// way. What it sees of each error is [`parts`]', as the printer sees it.
///
/// A `cosca` error is searched through its `source()`, so only a list's first member is. What is found
/// borrows from `e`.
pub(crate) fn find<'a, T>(e: &'a Dyn, found: &impl Fn(&'a Dyn) -> Option<T>) -> Option<T> {
    let node = parts(e);
    found(node.error)
        .or_else(|| node.source.and_then(|source| find(source, found)))
        .or_else(|| match &node.shape {
            Shape::Pieces(pieces) => pieces
                .errors()
                .filter(|(_, cause)| *cause)
                .find_map(|(error, _)| find(error, found)),
            Shape::Own | Shape::Cosca => None,
        })
}

/// The first OS error code in `e`, searched as [`find`] searches.
pub(crate) fn os_error_in(e: &Dyn) -> Option<i32> {
    find(e, &|e| e.downcast_ref::<std::io::Error>()?.raw_os_error())
}

/// Writes each of `remedies` with [`Sink::write_remedy`].
pub(super) fn write_remedies(out: &mut Sink, remedies: &[&str]) -> fmt::Result {
    for remedy in remedies {
        out.write_remedy(remedy)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;
