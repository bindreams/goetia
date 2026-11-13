//! Printing an [`Error`]: its chain of causes, then each remedy found along it.

use std::fmt::{self, Write as _};

use super::escape::{write_escaped, write_escaped_keeping_layout};
use super::{Error, Sentence};

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
/// inside another's sentence is written to `Display`.
///
/// Not escaped: the text lands in an error's `Display`, which the sink that prints it escapes. The
/// report does not print a [`Sentence`] through this, but writes the embedded error
/// itself (see [`write_chain`]), so its remedies print once, last.
pub(crate) struct Chain<'a>(pub(crate) &'a (dyn std::error::Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_chain(self.0, &mut Sink::without_stderr(f), &mut Vec::new())
    }
}

/// Writes `e` and its causes, collecting the remedies met on the way into `remedies`, outermost
/// first and each once: a [`Both`](Error::Both)'s halves can carry the same one. An error `e` sets
/// inside its own sentence (see [`Error::sentence`]) is written there, with its remedies collected.
///
/// A `cosca` error and everything below it is written by [`cosca::error::Report`]: its `Display` is
/// one layer, and a `source()` walk shows only the first member of a list and never a `cleanup`.
pub(super) fn write_chain<'a>(
    e: &'a (dyn std::error::Error + 'static),
    out: &mut Sink,
    remedies: &mut Vec<&'a str>,
) -> fmt::Result {
    if e.is::<cosca::error::Error>() {
        return write!(out, "{}", cosca::error::Report::new(e));
    }
    // A `#[source]` of type `Box<Error>` reaches here as the box.
    let ours = e
        .downcast_ref::<Error>()
        .or_else(|| e.downcast_ref::<Box<Error>>().map(|b| &**b));
    // The one place a `Kept` is looked through by hand: a kept `Both` is a `Both`.
    if let Some(Error::Both { first, then }) = ours.map(Error::unkept) {
        write_chain(first.as_ref(), out, remedies)?;
        out.write_str("; then: ")?;
        return write_chain(then.as_ref(), out, remedies);
    }
    if let Some(remedy) = ours.and_then(Error::remedy)
        && !remedies.contains(&remedy)
    {
        remedies.push(remedy);
    }
    match ours.and_then(Error::sentence) {
        Some(sentence) => write_sentence(sentence, out, remedies)?,
        None => write!(out, "{e}")?,
    }
    if out.stderr
        && let Some(stderr) = ours.and_then(Error::stderr).filter(|stderr| !stderr.is_empty())
    {
        out.write_str(": ")?;
        out.write_layout(stderr)?;
    }
    if let Some(source) = e.source() {
        out.write_str(": ")?;
        write_chain(source, out, remedies)?;
    }
    Ok(())
}

/// The first OS error code in `e`: its own, then its source's, then that of what it embeds. A
/// [`Both`](Error::Both) is searched first half first; [`Error::Kept`] is searched as the error it
/// holds.
pub(crate) fn os_error_in(e: &(dyn std::error::Error + 'static)) -> Option<i32> {
    if let Some(code) = e
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
    {
        return Some(code);
    }
    let ours = e
        .downcast_ref::<Error>()
        .or_else(|| e.downcast_ref::<Box<Error>>().map(|b| &**b));
    match ours {
        Some(Error::Both { first, then }) => return os_error_in(first.as_ref()).or_else(|| os_error_in(then.as_ref())),
        Some(Error::Kept(kept)) => return os_error_in(kept.as_ref()),
        _ => {}
    }
    e.source().and_then(os_error_in).or_else(|| {
        let sentence = ours.and_then(Error::sentence)?;
        os_error_in(sentence.error.filter(|_| sentence.error_is_cause)?)
    })
}

/// Writes `sentence`: its error is written as any error is, so its remedies are collected into
/// `remedies`.
pub(super) fn write_sentence<'a>(sentence: Sentence<'a>, out: &mut Sink, remedies: &mut Vec<&'a str>) -> fmt::Result {
    out.write_str(&sentence.before)?;
    if let Some(error) = sentence.error {
        write_chain(error, out, remedies)?;
    }
    out.write_str(&sentence.after)
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
